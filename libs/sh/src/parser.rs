//! Recursive-descent parser for the supported Bash subset.

use alloc::string::String;
use alloc::vec::Vec;

use crate::lexer::{RedirOp, Token, Word};

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
    /// `while` or `until`.
    While(WhileCommand),
    /// `{ ...; }` or `( ... )`.
    Group(Script),
}

/// A simple command.
#[derive(Clone, Debug)]
pub struct SimpleCommand {
    /// Leading `NAME=value` assignments.
    pub assignments: Vec<(String, Word)>,
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
    /// Target word for file redirections.
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
    /// Words after `in`, or `None` for the (empty) positional list.
    pub words: Option<Vec<Word>>,
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

/// A syntax error.
#[derive(Clone, Debug)]
pub struct ParseError {
    /// Human-readable reason.
    pub message: &'static str,
}

impl ParseError {
    fn new(message: &'static str) -> Self {
        Self { message }
    }
}

/// Parses a token stream.
pub fn parse(tokens: &[Token]) -> Result<Script, ParseError> {
    let mut parser = Parser { tokens, index: 0 };
    let script = parser.parse_script(&[])?;
    if !parser.at_end() {
        return Err(ParseError::new("unexpected token"));
    }
    Ok(script)
}

struct Parser<'a> {
    tokens: &'a [Token],
    index: usize,
}

impl Parser<'_> {
    fn at_end(&self) -> bool {
        self.index >= self.tokens.len()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.index).cloned();
        if token.is_some() {
            self.index += 1;
        }
        token
    }

    fn peek_word(&self) -> Option<&str> {
        match self.peek() {
            Some(Token::Word(word)) => word.plain(),
            _ => None,
        }
    }

    fn skip_separators(&mut self) {
        while matches!(self.peek(), Some(Token::Seq) | Some(Token::Newline)) {
            self.index += 1;
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(Token::Newline)) {
            self.index += 1;
        }
    }

    fn at_stop(&self, stops: &[&str]) -> bool {
        match self.peek_word() {
            Some(word) => stops.contains(&word),
            None => false,
        }
    }

    fn expect_word(&mut self, expected: &str) -> Result<(), ParseError> {
        if self.peek_word() == Some(expected) {
            self.index += 1;
            Ok(())
        } else {
            Err(ParseError::new("missing reserved word"))
        }
    }

    fn parse_script(&mut self, stops: &[&str]) -> Result<Script, ParseError> {
        let mut lists = Vec::new();
        loop {
            self.skip_separators();
            if self.at_end() {
                if stops.is_empty() {
                    break;
                }
                return Err(ParseError::new("unexpected end of input"));
            }
            if self.at_stop(stops) {
                break;
            }

            let list = self.parse_list()?;
            let background = list.background;
            lists.push(list);

            if self.at_end() || self.at_stop(stops) {
                break;
            }
            if background {
                self.skip_separators();
                continue;
            }
            match self.peek() {
                Some(Token::Seq) | Some(Token::Newline) => {}
                _ => return Err(ParseError::new("unexpected token")),
            }
        }
        Ok(Script { lists })
    }

    fn parse_list(&mut self) -> Result<List, ParseError> {
        let first = self.parse_pipeline()?;
        let mut rest = Vec::new();
        loop {
            let logic = match self.peek() {
                Some(Token::And) => Logic::And,
                Some(Token::Or) => Logic::Or,
                _ => break,
            };
            self.index += 1;
            self.skip_newlines();
            if self.at_end() {
                return Err(ParseError::new("missing command after operator"));
            }
            rest.push((logic, self.parse_pipeline()?));
        }

        let background = if matches!(self.peek(), Some(Token::Amp)) {
            self.index += 1;
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
        if self.peek_word() == Some("!") {
            self.index += 1;
            negate = true;
        }

        let mut commands = Vec::new();
        commands.push(self.parse_command()?);
        while matches!(self.peek(), Some(Token::Pipe)) {
            self.index += 1;
            self.skip_newlines();
            if self.at_end() {
                return Err(ParseError::new("missing command after pipe"));
            }
            commands.push(self.parse_command()?);
        }
        Ok(Pipeline { negate, commands })
    }

    fn parse_command(&mut self) -> Result<Command, ParseError> {
        match self.peek_word() {
            Some("if") => self.parse_if(),
            Some("for") => self.parse_for(),
            Some("while") => self.parse_while(false),
            Some("until") => self.parse_while(true),
            Some("{") => {
                self.index += 1;
                let body = self.parse_script(&["}"])?;
                self.expect_word("}")?;
                Ok(Command::Group(body))
            }
            Some("(") => {
                self.index += 1;
                let body = self.parse_script(&[")"])?;
                self.expect_word(")")?;
                Ok(Command::Group(body))
            }
            _ => self.parse_simple(),
        }
    }

    fn parse_simple(&mut self) -> Result<Command, ParseError> {
        let mut assignments = Vec::new();
        let mut words = Vec::new();
        let mut redirs = Vec::new();

        loop {
            match self.peek() {
                Some(Token::Redir(op)) => {
                    let op = *op;
                    self.index += 1;
                    let target = if matches!(op, RedirOp::Dup(_, _)) {
                        None
                    } else {
                        match self.next() {
                            Some(Token::Word(word)) => Some(word),
                            _ => return Err(ParseError::new("missing redirection target")),
                        }
                    };
                    redirs.push(Redirection { op, target });
                }
                Some(Token::Word(_)) => {
                    let Some(Token::Word(word)) = self.next() else {
                        unreachable!()
                    };
                    if words.is_empty() {
                        if let Some((name, value)) = word.split_assignment() {
                            assignments.push((name, value));
                            continue;
                        }
                    }
                    words.push(word);
                }
                _ => break,
            }
        }

        if words.is_empty() && assignments.is_empty() && redirs.is_empty() {
            return Err(ParseError::new("expected a command"));
        }
        Ok(Command::Simple(SimpleCommand {
            assignments,
            words,
            redirs,
        }))
    }

    fn parse_if(&mut self) -> Result<Command, ParseError> {
        self.expect_word("if")?;
        let condition = self.parse_script(&["then"])?;
        self.expect_word("then")?;
        let body = self.parse_script(&["elif", "else", "fi"])?;
        let mut branches = alloc::vec![(condition, body)];

        while self.peek_word() == Some("elif") {
            self.index += 1;
            let condition = self.parse_script(&["then"])?;
            self.expect_word("then")?;
            let body = self.parse_script(&["elif", "else", "fi"])?;
            branches.push((condition, body));
        }

        let else_body = if self.peek_word() == Some("else") {
            self.index += 1;
            Some(self.parse_script(&["fi"])?)
        } else {
            None
        };
        self.expect_word("fi")?;
        Ok(Command::If(IfCommand {
            branches,
            else_body,
        }))
    }

    fn parse_for(&mut self) -> Result<Command, ParseError> {
        self.expect_word("for")?;
        let Some(variable) = self.peek_word() else {
            return Err(ParseError::new("missing loop variable"));
        };
        let variable = String::from(variable);
        self.index += 1;

        let words = if self.peek_word() == Some("in") {
            self.index += 1;
            let mut words = Vec::new();
            while let Some(Token::Word(_)) = self.peek() {
                let Some(Token::Word(word)) = self.next() else {
                    unreachable!()
                };
                words.push(word);
            }
            Some(words)
        } else {
            None
        };

        self.skip_separators();
        self.expect_word("do")?;
        let body = self.parse_script(&["done"])?;
        self.expect_word("done")?;
        Ok(Command::For(ForCommand {
            variable,
            words,
            body,
        }))
    }

    fn parse_while(&mut self, until: bool) -> Result<Command, ParseError> {
        self.index += 1;
        let condition = self.parse_script(&["do"])?;
        self.expect_word("do")?;
        let body = self.parse_script(&["done"])?;
        self.expect_word("done")?;
        Ok(Command::While(WhileCommand {
            condition,
            body,
            until,
        }))
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
}
