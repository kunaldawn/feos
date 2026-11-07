//! Recursive-descent parser for the supported Bash subset.
//!
//! A [`Parser`] pulls one logical line at a time from the [`Lexer`], so a script
//! runs one command at a time: an alias defined on one line works on the next,
//! and a syntax error further down does not stop what came before it, exactly
//! as in other shells. Aliases expand here, at the token level, so an alias may
//! hold a whole `cd x && ls`. Nesting is bounded so that hostile input cannot
//! overflow a small kernel stack, both while parsing and later while walking or
//! dropping the tree.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::lexer::{valid_name, Assignment, Lexer, RedirOp, Token, Word};

/// Deepest nesting of compound commands the parser accepts.
pub const MAX_PARSE_DEPTH: usize = 32;

/// The error for nesting past the limits, which the executor treats like any
/// other nesting failure.
pub const TOO_DEEP: &str = "commands are nested too deeply";

/// How many alias substitutions one command position may go through.
const MAX_ALIAS_CHAIN: usize = 16;

/// A parsed script.
#[derive(Clone, Debug, Default)]
pub struct Script {
    /// Lists separated by `;`, newlines or `&`.
    pub lists: Vec<List>,
}

/// One `&&`/`||` chain.
#[derive(Clone, Debug)]
pub struct List {
    /// First pipeline.
    pub first: Pipeline,
    /// Following pipelines with their connecting operator.
    pub rest: Vec<(Logic, Pipeline)>,
    /// True when the list ended with `&`.
    pub background: bool,
}

/// Logical connector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Logic {
    /// `&&`
    And,
    /// `||`
    Or,
}

/// A pipeline of one or more commands.
#[derive(Clone, Debug)]
pub struct Pipeline {
    /// `!` negates the pipeline status.
    pub negate: bool,
    /// `time` reports how long the pipeline took.
    pub timed: bool,
    /// `time -p`: the report in POSIX's format.
    pub timed_posix: bool,
    /// Commands joined by `|`.
    pub commands: Vec<Command>,
}

/// A command in a pipeline.
#[derive(Clone, Debug)]
pub enum Command {
    /// A simple command: assignments, words and redirections.
    Simple(SimpleCommand),
    /// `if`/`elif`/`else`.
    If(IfCommand),
    /// `for`.
    For(ForCommand),
    /// `for ((init; test; step))`.
    ArithFor(ArithForCommand),
    /// `while` or `until`.
    While(WhileCommand),
    /// `case`.
    Case(CaseCommand),
    /// `{ ...; }`.
    Group(Script),
    /// `( ... )`.
    Subshell(Script),
    /// `(( expression ))`.
    Arith(String),
    /// `[[ expression ]]`.
    Cond(CondExpr),
    /// `name() body`.
    Function(FunctionDef),
    /// A compound command with redirections.
    Redirected(Box<Command>, Vec<Redirection>),
}

/// A simple command.
#[derive(Clone, Debug)]
pub struct SimpleCommand {
    /// Leading `NAME=value` assignments.
    pub assignments: Vec<Assignment>,
    /// Command name and arguments.
    pub words: Vec<Word>,
    /// Redirections in source order.
    pub redirs: Vec<Redirection>,
}

/// One redirection.
#[derive(Clone, Debug)]
pub struct Redirection {
    /// Operator.
    pub op: RedirOp,
    /// Target word for file redirections, or the here-document body.
    pub target: Option<Word>,
}

/// `if` command.
#[derive(Clone, Debug)]
pub struct IfCommand {
    /// `(condition, body)` pairs.
    pub branches: Vec<(Script, Script)>,
    /// Optional `else` body.
    pub else_body: Option<Script>,
}

/// `for` command.
#[derive(Clone, Debug)]
pub struct ForCommand {
    /// Loop variable.
    pub variable: String,
    /// Words after `in`, or `None` for the positional parameters.
    pub words: Option<Vec<Word>>,
    /// Body.
    pub body: Script,
}

/// `for ((init; test; step))` command.
#[derive(Clone, Debug)]
pub struct ArithForCommand {
    /// Runs once before the loop.
    pub init: String,
    /// Tested before each pass; empty means true.
    pub test: String,
    /// Runs after each pass.
    pub step: String,
    /// Body.
    pub body: Script,
}

/// `while`/`until` command.
#[derive(Clone, Debug)]
pub struct WhileCommand {
    /// Condition script.
    pub condition: Script,
    /// Body.
    pub body: Script,
    /// True for `until`.
    pub until: bool,
}

/// How a `case` branch ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseFlow {
    /// `;;`: leave the `case`.
    End,
    /// `;&`: run the next branch's body.
    FallThrough,
    /// `;;&`: keep testing the next patterns.
    Continue,
}

/// One `case` branch.
#[derive(Clone, Debug)]
pub struct CaseItem {
    /// Patterns separated by `|`.
    pub patterns: Vec<Word>,
    /// Body.
    pub body: Script,
    /// What happens after the body.
    pub flow: CaseFlow,
}

/// `case` command.
#[derive(Clone, Debug)]
pub struct CaseCommand {
    /// The tested word.
    pub word: Word,
    /// Branches in order.
    pub items: Vec<CaseItem>,
}

/// `name() { ...; }`.
#[derive(Clone, Debug)]
pub struct FunctionDef {
    /// Function name.
    pub name: String,
    /// Body, shared cheaply with shell snapshots.
    pub body: Arc<Command>,
}

/// A `[[ ]]` expression.
#[derive(Clone, Debug)]
pub enum CondExpr {
    /// A lone word: true when non-empty.
    Word(Word),
    /// `-f word`.
    Unary(String, Word),
    /// `left op right`.
    Binary(Word, String, Word),
    /// `! expr`.
    Not(Box<CondExpr>),
    /// `a && b`.
    And(Box<CondExpr>, Box<CondExpr>),
    /// `a || b`.
    Or(Box<CondExpr>, Box<CondExpr>),
}

/// A syntax error.
#[derive(Clone, Debug)]
pub struct ParseError {
    /// Human-readable reason.
    pub message: &'static str,
    /// True when reading more input could complete the construct.
    pub incomplete: bool,
}

impl ParseError {
    fn new(message: &'static str) -> Self {
        Self {
            message,
            incomplete: false,
        }
    }

    fn more(message: &'static str) -> Self {
        Self {
            message,
            incomplete: true,
        }
    }
}

/// Parses a token stream.
pub fn parse(tokens: &[Token]) -> Result<Script, ParseError> {
    let mut parser = Parser {
        queue: tokens.iter().cloned().collect(),
        lexer: None,
        aliases: None,
        depth: 0,
        stack_bound: None,
        expand_after: 0,
    };
    let mut lists = Vec::new();
    while let Some(list) = parser.parse_top()? {
        lists.push(list);
    }
    Ok(Script { lists })
}

/// Tests whether `text` is a complete script. `Err(true)` means more input
/// could finish it, and `Err(false)` means it is simply wrong.
pub fn check_complete(text: &str) -> Result<(), bool> {
    let mut parser = Parser::new(text);
    loop {
        match parser.next_list(None) {
            Ok(Some(_)) => {}
            Ok(None) => return Ok(()),
            Err(error) => return Err(error.incomplete),
        }
    }
}

/// An incremental parser over script text.
pub struct Parser<'a> {
    queue: VecDeque<Token>,
    lexer: Option<Lexer>,
    aliases: Option<&'a BTreeMap<String, String>>,
    depth: usize,
    stack_bound: Option<(usize, usize)>,
    expand_after: usize,
}

impl<'a> Parser<'a> {
    /// Creates a parser over `text`.
    pub fn new(text: &str) -> Parser<'static> {
        Parser {
            queue: VecDeque::new(),
            lexer: Some(Lexer::new(text)),
            aliases: None,
            depth: 0,
            stack_bound: None,
            expand_after: 0,
        }
    }

    /// Makes nesting fail once the stack has moved `limit` bytes from `base`,
    /// for a parser that may start on an already deep stack.
    pub fn set_stack_bound(&mut self, base: usize, limit: usize) {
        self.stack_bound = Some((base, limit));
    }
}

impl Parser<'_> {
    /// Returns the next complete top-level list, or `None` at the end.
    pub fn next_list(
        &mut self,
        aliases: Option<&BTreeMap<String, String>>,
    ) -> Result<Option<List>, ParseError> {
        // The map is only borrowed for this call: the list it produces may run
        // and change it before the next one is parsed.
        let mut scoped = Parser {
            queue: core::mem::take(&mut self.queue),
            lexer: self.lexer.take(),
            aliases,
            depth: 0,
            stack_bound: self.stack_bound,
            expand_after: 0,
        };
        let result = scoped.parse_top();
        self.queue = scoped.queue;
        self.lexer = scoped.lexer;
        result
    }
}

impl Parser<'_> {
    /// Tops the queue up to at least `count` tokens when more text remains.
    fn fill(&mut self, count: usize) -> Result<(), ParseError> {
        while self.queue.len() < count {
            let Some(lexer) = self.lexer.as_mut() else {
                return Ok(());
            };
            if lexer.at_end() {
                return Ok(());
            }
            let mut tokens = Vec::new();
            lexer.next_line(&mut tokens).map_err(|error| ParseError {
                message: error.message,
                incomplete: error.incomplete,
            })?;
            self.queue.extend(tokens);
        }
        Ok(())
    }

    fn peek(&mut self) -> Result<Option<&Token>, ParseError> {
        self.fill(1)?;
        Ok(self.queue.front())
    }

    fn peek_at(&mut self, index: usize) -> Result<Option<&Token>, ParseError> {
        self.fill(index + 1)?;
        Ok(self.queue.get(index))
    }

    fn advance(&mut self) -> Option<Token> {
        self.queue.pop_front()
    }

    fn at_end(&mut self) -> Result<bool, ParseError> {
        Ok(self.peek()?.is_none())
    }

    fn peek_word(&mut self) -> Result<Option<&str>, ParseError> {
        Ok(match self.peek()? {
            Some(Token::Word(word)) => word.plain(),
            _ => None,
        })
    }

    fn skip_separators(&mut self) -> Result<(), ParseError> {
        while matches!(self.peek()?, Some(Token::Seq | Token::Newline)) {
            self.advance();
        }
        Ok(())
    }

    fn skip_newlines(&mut self) -> Result<(), ParseError> {
        while matches!(self.peek()?, Some(Token::Newline)) {
            self.advance();
        }
        Ok(())
    }

    fn expect_word(&mut self, expected: &str) -> Result<(), ParseError> {
        if self.peek_word()? == Some(expected) {
            self.advance();
            Ok(())
        } else if self.at_end()? {
            Err(ParseError::more("unexpected end of input"))
        } else {
            Err(ParseError::new("missing reserved word"))
        }
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            return Err(ParseError::new(TOO_DEEP));
        }
        if let Some((base, limit)) = self.stack_bound {
            if base.abs_diff(crate::exec::stack_position()) > limit {
                return Err(ParseError::new(TOO_DEEP));
            }
        }
        Ok(())
    }

    fn parse_top(&mut self) -> Result<Option<List>, ParseError> {
        self.skip_separators()?;
        if self.at_end()? {
            return Ok(None);
        }
        let list = self.parse_list()?;
        match self.peek()? {
            None | Some(Token::Seq | Token::Newline) => {}
            Some(_) if list.background => {}
            Some(_) => return Err(ParseError::new("unexpected token")),
        }
        Ok(Some(list))
    }

    fn at_stop(&mut self, stops: &Stops<'_>) -> Result<bool, ParseError> {
        let at_case = stops.case;
        let at_paren = stops.paren;
        Ok(match self.peek()? {
            Some(Token::RParen) => at_paren,
            Some(Token::CaseEnd | Token::CaseFall | Token::CaseContinue) => at_case,
            Some(Token::Word(word)) => word.plain().is_some_and(|text| stops.words.contains(&text)),
            _ => false,
        })
    }

    fn parse_script(&mut self, stops: &Stops<'_>) -> Result<Script, ParseError> {
        let mut lists = Vec::new();
        loop {
            self.skip_separators()?;
            if self.at_end()? {
                return Err(ParseError::more("unexpected end of input"));
            }
            if self.at_stop(stops)? {
                break;
            }

            let list = self.parse_list()?;
            let background = list.background;
            lists.push(list);

            if self.at_end()? {
                return Err(ParseError::more("unexpected end of input"));
            }
            if self.at_stop(stops)? {
                break;
            }
            if background {
                continue;
            }
            match self.peek()? {
                Some(Token::Seq | Token::Newline) => {}
                _ => return Err(ParseError::new("unexpected token")),
            }
        }
        Ok(Script { lists })
    }

    fn parse_list(&mut self) -> Result<List, ParseError> {
        let first = self.parse_pipeline()?;
        let mut rest = Vec::new();
        loop {
            let logic = match self.peek()? {
                Some(Token::And) => Logic::And,
                Some(Token::Or) => Logic::Or,
                _ => break,
            };
            self.advance();
            self.skip_newlines()?;
            if self.at_end()? {
                return Err(ParseError::more("missing command after operator"));
            }
            rest.push((logic, self.parse_pipeline()?));
        }

        let background = if matches!(self.peek()?, Some(Token::Amp)) {
            self.advance();
            true
        } else {
            false
        };
        Ok(List {
            first,
            rest,
            background,
        })
    }

    fn parse_pipeline(&mut self) -> Result<Pipeline, ParseError> {
        let mut negate = false;
        let mut timed = false;
        let mut timed_posix = false;
        // `time` is a reserved word here, not a command, so it covers a whole pipeline.
        while let Some(word) = self.peek_word()? {
            match word {
                "!" => negate = !negate,
                "time" if !timed => timed = true,
                "-p" if timed && !timed_posix => timed_posix = true,
                _ => break,
            }
            self.advance();
        }

        let mut commands = Vec::new();
        // `time` on its own times nothing, as it does in Bash.
        if timed && matches!(self.peek()?, None | Some(Token::Newline | Token::Seq)) {
            return Ok(Pipeline { negate, timed, timed_posix, commands });
        }
        commands.push(self.parse_command()?);
        loop {
            let both = match self.peek()? {
                Some(Token::Pipe) => false,
                Some(Token::PipeBoth) => true,
                _ => break,
            };
            self.advance();
            self.skip_newlines()?;
            if self.at_end()? {
                return Err(ParseError::more("missing command after pipe"));
            }
            if both {
                if let Some(last) = commands.pop() {
                    let dup = Redirection {
                        op: RedirOp::Dup(2, 1),
                        target: None,
                    };
                    commands.push(Command::Redirected(Box::new(last), alloc::vec![dup]));
                }
            }
            commands.push(self.parse_command()?);
        }
        Ok(Pipeline { negate, timed, timed_posix, commands })
    }

    /// Replaces an alias at the front of the queue with its tokens.
    fn expand_alias(&mut self, seen: &mut Vec<String>) -> Result<bool, ParseError> {
        let Some(aliases) = self.aliases else {
            return Ok(false);
        };
        let name = match self.peek()? {
            Some(Token::Word(word)) => match word.plain() {
                Some(text) => String::from(text),
                None => return Ok(false),
            },
            _ => return Ok(false),
        };
        let Some(value) = aliases.get(&name) else {
            return Ok(false);
        };
        if seen.contains(&name) || seen.len() >= MAX_ALIAS_CHAIN {
            return Ok(false);
        }
        // `name()` defines a function, whatever `name` means as an alias.
        if matches!(self.peek_at(1)?, Some(Token::LParen)) {
            return Ok(false);
        }

        let tokens = crate::lexer::lex(value).map_err(|error| ParseError {
            message: error.message,
            incomplete: false,
        })?;
        // A trailing blank makes the word after the alias eligible too.
        self.expand_after = if value.ends_with(' ') { tokens.len() } else { 0 };
        self.advance();
        for token in tokens.into_iter().rev() {
            self.queue.push_front(token);
        }
        seen.push(name);
        Ok(true)
    }

    fn parse_command(&mut self) -> Result<Command, ParseError> {
        self.enter()?;
        let mut seen = Vec::new();
        while self.expand_alias(&mut seen)? {}
        let command = self.parse_command_inner();
        self.depth -= 1;
        command
    }

    fn parse_command_inner(&mut self) -> Result<Command, ParseError> {
        if matches!(self.peek()?, Some(Token::Arith(_))) {
            let Some(Token::Arith(text)) = self.advance() else {
                return Err(ParseError::new("unexpected token"));
            };
            return self.parse_redirects(Command::Arith(text));
        }
        if matches!(self.peek()?, Some(Token::LParen)) {
            self.advance();
            let stops = Stops {
                words: &[],
                paren: true,
                case: false,
            };
            let body = self.parse_script(&stops)?;
            self.advance();
            return self.parse_redirects(Command::Subshell(body));
        }

        let word = self.peek_word()?.map(String::from);
        if matches!(
            word.as_deref(),
            Some("then" | "elif" | "else" | "fi" | "do" | "done" | "esac" | "}")
        ) {
            return Err(ParseError::new("unexpected reserved word"));
        }
        let compound = match word.as_deref() {
            Some("if") => Some(self.parse_if()?),
            Some("for") => Some(self.parse_for()?),
            Some("while") => Some(self.parse_while(false)?),
            Some("until") => Some(self.parse_while(true)?),
            Some("case") => Some(self.parse_case()?),
            Some("[[") => Some(self.parse_cond_command()?),
            Some("{") => {
                self.advance();
                let stops = Stops::words(&["}"]);
                let body = self.parse_script(&stops)?;
                self.expect_word("}")?;
                Some(Command::Group(body))
            }
            Some("function") => {
                self.advance();
                let Some(name) = self.peek_word()?.map(String::from) else {
                    return if self.at_end()? {
                        Err(ParseError::more("missing function name"))
                    } else {
                        Err(ParseError::new("invalid function name"))
                    };
                };
                if !valid_name(&name) {
                    return Err(ParseError::new("invalid function name"));
                }
                self.advance();
                if matches!(self.peek()?, Some(Token::LParen)) {
                    self.advance();
                    if !matches!(self.peek()?, Some(Token::RParen)) {
                        return Err(ParseError::new("expected `)` after `(`"));
                    }
                    self.advance();
                }
                return self.parse_function_body(name);
            }
            _ => None,
        };
        if let Some(command) = compound {
            return self.parse_redirects(command);
        }

        // `name()` defines a function.
        if let Some(name) = word.as_deref().filter(|name| valid_name(name)) {
            if matches!(self.peek_at(1)?, Some(Token::LParen))
                && matches!(self.peek_at(2)?, Some(Token::RParen))
            {
                let name = String::from(name);
                self.advance();
                self.advance();
                self.advance();
                return self.parse_function_body(name);
            }
        }
        self.parse_simple()
    }

    fn parse_function_body(&mut self, name: String) -> Result<Command, ParseError> {
        self.skip_newlines()?;
        if self.at_end()? {
            return Err(ParseError::more("missing function body"));
        }
        let body = self.parse_command()?;
        if matches!(body, Command::Simple(_)) {
            return Err(ParseError::new("a function body must be a compound command"));
        }
        Ok(Command::Function(FunctionDef {
            name,
            body: Arc::new(body),
        }))
    }

    fn parse_redirection(&mut self, op: RedirOp) -> Result<Redirection, ParseError> {
        let target = if matches!(op, RedirOp::Dup(_, _) | RedirOp::Close(_)) {
            None
        } else {
            match self.peek()? {
                Some(Token::Word(_)) => match self.advance() {
                    Some(Token::Word(word)) => Some(word),
                    _ => None,
                },
                None => return Err(ParseError::more("missing redirection target")),
                Some(_) => return Err(ParseError::new("missing redirection target")),
            }
        };
        Ok(Redirection { op, target })
    }

    /// Collects redirections after a compound command.
    fn parse_redirects(&mut self, command: Command) -> Result<Command, ParseError> {
        let mut redirs = Vec::new();
        while let Some(Token::Redir(op)) = self.peek()? {
            let op = *op;
            self.advance();
            redirs.push(self.parse_redirection(op)?);
        }
        if redirs.is_empty() {
            Ok(command)
        } else {
            Ok(Command::Redirected(Box::new(command), redirs))
        }
    }

    fn parse_simple(&mut self) -> Result<Command, ParseError> {
        let mut assignments = Vec::new();
        let mut words = Vec::new();
        let mut redirs = Vec::new();
        let mut seen = Vec::new();

        loop {
            if words.is_empty() && !assignments.is_empty() {
                while self.expand_alias(&mut seen)? {}
            }
            match self.peek()? {
                Some(Token::Redir(op)) => {
                    let op = *op;
                    self.advance();
                    redirs.push(self.parse_redirection(op)?);
                }
                Some(Token::Word(_)) => {
                    let Some(Token::Word(word)) = self.advance() else {
                        return Err(ParseError::new("expected a word"));
                    };
                    if words.is_empty() {
                        if let Some(assignment) = word.split_assignment() {
                            assignments.push(assignment);
                            continue;
                        }
                    }
                    words.push(word);
                    if self.expand_after > 0 {
                        self.expand_after -= 1;
                        if self.expand_after == 0 {
                            while self.expand_alias(&mut seen)? {}
                        }
                    }
                }
                _ => break,
            }
        }

        if words.is_empty() && assignments.is_empty() && redirs.is_empty() {
            return if self.at_end()? {
                Err(ParseError::more("expected a command"))
            } else {
                Err(ParseError::new("expected a command"))
            };
        }
        Ok(Command::Simple(SimpleCommand {
            assignments,
            words,
            redirs,
        }))
    }

    fn parse_if(&mut self) -> Result<Command, ParseError> {
        self.expect_word("if")?;
        let condition = self.parse_script(&Stops::words(&["then"]))?;
        self.expect_word("then")?;
        let body = self.parse_script(&Stops::words(&["elif", "else", "fi"]))?;
        let mut branches = alloc::vec![(condition, body)];

        while self.peek_word()? == Some("elif") {
            self.advance();
            let condition = self.parse_script(&Stops::words(&["then"]))?;
            self.expect_word("then")?;
            let body = self.parse_script(&Stops::words(&["elif", "else", "fi"]))?;
            branches.push((condition, body));
        }

        let else_body = if self.peek_word()? == Some("else") {
            self.advance();
            Some(self.parse_script(&Stops::words(&["fi"]))?)
        } else {
            None
        };
        self.expect_word("fi")?;
        Ok(Command::If(IfCommand {
            branches,
            else_body,
        }))
    }

    fn parse_body(&mut self) -> Result<Script, ParseError> {
        self.expect_word("do")?;
        let body = self.parse_script(&Stops::words(&["done"]))?;
        self.expect_word("done")?;
        Ok(body)
    }

    fn parse_for(&mut self) -> Result<Command, ParseError> {
        self.expect_word("for")?;

        if matches!(self.peek()?, Some(Token::ArithFor(_))) {
            let Some(Token::ArithFor(text)) = self.advance() else {
                return Err(ParseError::new("unexpected token"));
            };
            let mut parts = text.splitn(3, ';');
            let init = String::from(parts.next().unwrap_or(""));
            let test = String::from(parts.next().unwrap_or(""));
            let Some(step) = parts.next() else {
                return Err(ParseError::new("for: expected `;` in arithmetic loop"));
            };
            let step = String::from(step);
            self.skip_separators()?;
            let body = self.parse_body()?;
            return Ok(Command::ArithFor(ArithForCommand {
                init,
                test,
                step,
                body,
            }));
        }

        let Some(variable) = self.peek_word()?.map(String::from) else {
            return if self.at_end()? {
                Err(ParseError::more("missing loop variable"))
            } else {
                Err(ParseError::new("missing loop variable"))
            };
        };
        if !valid_name(&variable) {
            return Err(ParseError::new("invalid loop variable"));
        }
        self.advance();

        self.skip_newlines()?;
        let words = if self.peek_word()? == Some("in") {
            self.advance();
            let mut words = Vec::new();
            while let Some(Token::Word(_)) = self.peek()? {
                let Some(Token::Word(word)) = self.advance() else {
                    return Err(ParseError::new("expected a word"));
                };
                words.push(word);
            }
            Some(words)
        } else {
            None
        };

        self.skip_separators()?;
        let body = self.parse_body()?;
        Ok(Command::For(ForCommand {
            variable,
            words,
            body,
        }))
    }

    fn parse_while(&mut self, until: bool) -> Result<Command, ParseError> {
        self.advance();
        let condition = self.parse_script(&Stops::words(&["do"]))?;
        let body = self.parse_body()?;
        Ok(Command::While(WhileCommand {
            condition,
            body,
            until,
        }))
    }

    fn parse_case(&mut self) -> Result<Command, ParseError> {
        self.expect_word("case")?;
        let word = match self.peek()? {
            Some(Token::Word(_)) => match self.advance() {
                Some(Token::Word(word)) => word,
                _ => return Err(ParseError::new("expected a word after `case`")),
            },
            None => return Err(ParseError::more("expected a word after `case`")),
            Some(_) => return Err(ParseError::new("expected a word after `case`")),
        };
        self.skip_newlines()?;
        self.expect_word("in")?;

        let mut items = Vec::new();
        loop {
            self.skip_separators()?;
            if self.peek_word()? == Some("esac") {
                self.advance();
                break;
            }
            if self.at_end()? {
                return Err(ParseError::more("expected `esac`"));
            }
            if matches!(self.peek()?, Some(Token::LParen)) {
                self.advance();
            }

            let mut patterns = Vec::new();
            loop {
                match self.advance() {
                    Some(Token::Word(word)) => patterns.push(word),
                    None => return Err(ParseError::more("expected a pattern")),
                    Some(_) => return Err(ParseError::new("expected a pattern")),
                }
                match self.peek()? {
                    Some(Token::Pipe) => {
                        self.advance();
                    }
                    Some(Token::RParen) => {
                        self.advance();
                        break;
                    }
                    None => return Err(ParseError::more("expected `)` in `case`")),
                    Some(_) => return Err(ParseError::new("expected `)` in `case`")),
                }
            }

            let stops = Stops {
                words: &["esac"],
                paren: false,
                case: true,
            };
            let body = self.parse_script(&stops)?;
            let flow = match self.peek()? {
                Some(Token::CaseEnd) => CaseFlow::End,
                Some(Token::CaseFall) => CaseFlow::FallThrough,
                Some(Token::CaseContinue) => CaseFlow::Continue,
                _ => CaseFlow::End,
            };
            if !matches!(self.peek_word()?, Some("esac")) {
                self.advance();
            }
            items.push(CaseItem {
                patterns,
                body,
                flow,
            });
        }
        Ok(Command::Case(CaseCommand { word, items }))
    }

    fn parse_cond_command(&mut self) -> Result<Command, ParseError> {
        self.advance();
        let expr = self.cond_or()?;
        if self.peek_word()? != Some("]]") {
            return if self.at_end()? {
                Err(ParseError::more("expected `]]`"))
            } else {
                Err(ParseError::new("expected `]]`"))
            };
        }
        self.advance();
        Ok(Command::Cond(expr))
    }

    fn cond_or(&mut self) -> Result<CondExpr, ParseError> {
        let mut left = self.cond_and()?;
        while self.peek_word()? == Some("||") {
            self.advance();
            let right = self.cond_and()?;
            left = CondExpr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn cond_and(&mut self) -> Result<CondExpr, ParseError> {
        let mut left = self.cond_not()?;
        while self.peek_word()? == Some("&&") {
            self.advance();
            let right = self.cond_not()?;
            left = CondExpr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn cond_not(&mut self) -> Result<CondExpr, ParseError> {
        if self.peek_word()? == Some("!") {
            self.advance();
            self.enter()?;
            let inner = self.cond_not();
            self.depth -= 1;
            return Ok(CondExpr::Not(Box::new(inner?)));
        }
        self.cond_primary()
    }

    fn cond_word(&mut self) -> Result<Word, ParseError> {
        match self.peek()? {
            Some(Token::Word(_)) => match self.advance() {
                Some(Token::Word(word)) => Ok(word),
                _ => Err(ParseError::new("expected a word in `[[`")),
            },
            None => Err(ParseError::more("expected a word in `[[`")),
            Some(_) => Err(ParseError::new("expected a word in `[[`")),
        }
    }

    fn cond_primary(&mut self) -> Result<CondExpr, ParseError> {
        if self.peek_word()? == Some("(") {
            self.advance();
            self.enter()?;
            let inner = self.cond_or();
            self.depth -= 1;
            let inner = inner?;
            if self.peek_word()? != Some(")") {
                return Err(ParseError::new("expected `)` in `[[`"));
            }
            self.advance();
            return Ok(inner);
        }

        let first = self.cond_word()?;
        if let Some(operator) = first.plain().filter(|word| is_cond_unary(word)) {
            let operator = String::from(operator);
            if !matches!(self.peek_word()?, Some("]]" | "&&" | "||" | ")")) {
                let operand = self.cond_word()?;
                return Ok(CondExpr::Unary(operator, operand));
            }
        }
        if let Some(operator) = self.peek_word()?.filter(|word| is_cond_binary(word)) {
            let operator = String::from(operator);
            self.advance();
            let right = self.cond_word()?;
            return Ok(CondExpr::Binary(first, operator, right));
        }
        Ok(CondExpr::Word(first))
    }
}

fn is_cond_unary(word: &str) -> bool {
    matches!(
        word,
        "-e" | "-f" | "-d" | "-s" | "-r" | "-w" | "-x" | "-z" | "-n" | "-L" | "-h" | "-a" | "-v"
    )
}

fn is_cond_binary(word: &str) -> bool {
    matches!(
        word,
        "=" | "==" | "!=" | "=~" | "<" | ">" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge"
    )
}

/// What ends a nested command list.
struct Stops<'a> {
    words: &'a [&'a str],
    paren: bool,
    case: bool,
}

impl<'a> Stops<'a> {
    const fn words(words: &'a [&'a str]) -> Self {
        Self {
            words,
            paren: false,
            case: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn parsed(text: &str) -> Script {
        parse(&lex(text).unwrap()).unwrap()
    }

    #[test]
    fn parses_lists_and_pipelines() {
        let script = parsed("echo a | grep a && echo yes; echo done");
        assert_eq!(script.lists.len(), 2);
        assert_eq!(script.lists[0].first.commands.len(), 2);
        assert_eq!(script.lists[0].rest.len(), 1);
        assert_eq!(script.lists[0].rest[0].0, Logic::And);
    }

    #[test]
    fn parses_redirections_and_assignments() {
        let script = parsed("FOO=bar cat < in > out 2>&1");
        let Command::Simple(simple) = &script.lists[0].first.commands[0] else {
            panic!("expected a simple command");
        };
        assert_eq!(simple.assignments.len(), 1);
        assert_eq!(simple.words.len(), 1);
        assert_eq!(simple.redirs.len(), 3);
        assert_eq!(simple.redirs[2].op, RedirOp::Dup(2, 1));
    }

    #[test]
    fn parses_compound_commands() {
        let script = parsed("if true; then echo yes; else echo no; fi");
        assert!(matches!(script.lists[0].first.commands[0], Command::If(_)));

        let script = parsed("for x in a b; do echo $x; done");
        assert!(matches!(script.lists[0].first.commands[0], Command::For(_)));

        let script = parsed("while false; do echo loop; done");
        assert!(matches!(
            script.lists[0].first.commands[0],
            Command::While(_)
        ));

        let script = parsed("(echo sub)");
        assert!(matches!(
            script.lists[0].first.commands[0],
            Command::Subshell(_)
        ));

        let script = parsed("case $x in a|b) echo y;; (c) echo z;& *) echo w;; esac");
        let Command::Case(case) = &script.lists[0].first.commands[0] else {
            panic!("expected case");
        };
        assert_eq!(case.items.len(), 3);
        assert_eq!(case.items[0].patterns.len(), 2);
        assert_eq!(case.items[1].flow, CaseFlow::FallThrough);

        assert!(matches!(
            parsed("[[ -f a && b == c ]]").lists[0].first.commands[0],
            Command::Cond(CondExpr::And(_, _))
        ));
        assert!(matches!(
            parsed("((1 < 2))").lists[0].first.commands[0],
            Command::Arith(_)
        ));
    }

    #[test]
    fn parses_function_definitions() {
        for text in ["f() { echo a; }", "function f { echo a; }", "function f() (echo a)"] {
            let script = parsed(text);
            let Command::Function(def) = &script.lists[0].first.commands[0] else {
                panic!("expected a function for {text}");
            };
            assert_eq!(def.name, "f");
        }
        assert!(parse(&lex("f() echo a").unwrap()).is_err());
    }

    #[test]
    fn compound_commands_take_redirections() {
        let script = parsed("while read l; do echo $l; done < in > out");
        let Command::Redirected(inner, redirs) = &script.lists[0].first.commands[0] else {
            panic!("expected redirections");
        };
        assert!(matches!(**inner, Command::While(_)));
        assert_eq!(redirs.len(), 2);
    }

    #[test]
    fn parses_background_lists() {
        let script = parsed("sleep 1 & echo done");
        assert_eq!(script.lists.len(), 2);
        assert!(script.lists[0].background);
    }

    #[test]
    fn reports_missing_terminators() {
        assert!(parse(&lex("if true; then echo").unwrap()).is_err());
        assert!(parse(&lex("for x in a; do").unwrap()).is_err());
        assert!(parse(&lex("echo a |").unwrap()).is_err());
    }

    #[test]
    fn knows_which_errors_more_input_can_fix() {
        for text in [
            "if true; then",
            "for i in 1 2; do",
            "while :; do",
            "echo a |",
            "echo a &&",
            "{ echo a",
            "(echo a",
            "f() {",
            "case x in",
            "case x in a) echo y;;",
            "echo 'open",
            "echo \"open",
            "echo \\",
            "cat <<EOF\nbody",
            "[[ a ==",
        ] {
            assert!(crate::lexer::needs_more(text), "{text}");
        }
        for text in ["echo a", "if true; then echo a; fi", "echo )", "fi", "done"] {
            assert!(!crate::lexer::needs_more(text), "{text}");
        }
    }

    #[test]
    fn deep_nesting_is_an_error() {
        let text = "(".repeat(2000);
        assert!(parse(&lex(&text).unwrap()).is_err());
        let text = "if true; then ".repeat(2000);
        assert!(parse(&lex(&text).unwrap()).is_err());
        let text = "{ ".repeat(2000);
        assert!(check_complete(&text).is_err());
        let text = alloc::format!("[[ {} a ]]", "( ".repeat(2000));
        assert!(check_complete(&text).is_err());
    }

    #[test]
    fn aliases_expand_as_tokens() {
        let mut aliases = BTreeMap::new();
        aliases.insert(String::from("x"), String::from("cd /d && ls"));
        aliases.insert(String::from("loop"), String::from("loop -l"));
        let mut parser = Parser::new("x arg\nloop");
        let list = parser.next_list(Some(&aliases)).unwrap().unwrap();
        assert_eq!(list.rest.len(), 1);
        let list = parser.next_list(Some(&aliases)).unwrap().unwrap();
        let Command::Simple(simple) = &list.first.commands[0] else {
            panic!();
        };
        assert_eq!(simple.words.len(), 2);
    }

    #[test]
    fn scripts_parse_one_command_at_a_time() {
        let mut parser = Parser::new("echo ok\nfi\n");
        assert!(parser.next_list(None).unwrap().is_some());
        assert!(parser.next_list(None).is_err());
    }
}
