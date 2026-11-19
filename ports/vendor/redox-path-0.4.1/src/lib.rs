#![no_std]

extern crate alloc;

use alloc::{
    borrow::Cow,
    format,
    string::{String, ToString},
};
use core::{ffi::CStr, fmt};

/// The default scheme. Currently hardcoded.
const DEFAULT_SCHEME: &str = "file";

/// `PATH_MAX` constant used in this crate
pub const PATH_MAX: usize = 4096;

/// The name of a scheme
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RedoxScheme<'a>(Cow<'a, str>);

impl<'a> RedoxScheme<'a> {
    /// Create a new [`RedoxScheme`], ensuring there are no invalid characters
    pub fn new<S: Into<Cow<'a, str>>>(scheme: S) -> Option<Self> {
        let scheme = scheme.into();
        // Scheme cannot have NUL, `/`, `:` or non ascii characters
        // SIMD: schemes are short, so don't use memchr to avoid warmup penalty
        //       is_ascii() from std is SSE2 optimized only if str is long enough
        if !scheme.is_ascii()
            || scheme
                .as_bytes()
                .iter()
                .any(|b| matches!(b, b'\0' | b'/' | b':'))
        {
            return None;
        }
        Some(Self(scheme))
    }

    /// Similar to [`canonicalize_using_scheme`]
    pub fn canonicalize_as_scheme<'b>(&self, path: RedoxStr<'b>) -> RedoxPath<'b> {
        canonicalize_using_scheme_checked(self, path)
    }

    pub fn into_owned<'b>(self) -> RedoxScheme<'b> {
        RedoxScheme(self.0.into_owned().into())
    }
}

impl<'a> AsRef<str> for RedoxScheme<'a> {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl<'a> fmt::Display for RedoxScheme<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The part of a path that is sent to each scheme
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RedoxReference<'a>(Cow<'a, str>);

impl<'a> RedoxReference<'a> {
    /// Create a new [`RedoxReference`], ensuring there are no invalid characters.
    /// An empty path is considered valid and equivalent to '/'.
    /// A path not starting with '/' will be considered not canonical.
    pub fn new<S: Into<Cow<'a, str>>>(reference: S) -> Option<Self> {
        let reference = reference.into();
        // Reference cannot have NUL
        // as_bytes() usage is safe: https://stackoverflow.com/a/6907327/3908409
        #[cfg(not(feature = "simd"))]
        if reference.as_bytes().iter().any(|&b| b == b'\0') {
            return None;
        }
        #[cfg(feature = "simd")]
        if memchr::memchr(b'\0', reference.as_bytes()).is_some() {
            return None;
        }
        Some(Self(reference))
    }

    /// Create a new [`RedoxReference`] from C string, ensuring it is a valid UTF-8
    pub fn new_from_c<S: Into<&'a CStr>>(reference: S) -> Option<Self> {
        let reference = reference.into();
        #[cfg(feature = "simd")]
        let s = simdutf8::basic::from_utf8(reference.to_bytes()).ok();
        #[cfg(not(feature = "simd"))]
        let s = reference.to_str().ok();
        Some(Self(Cow::Borrowed(s?)))
    }

    /// Create a [`RedoxReference`] from a buf, returns [`None`] if it not a valid UTF-8 or valid from NUL
    pub fn new_from_buf(path: &'a [u8], len: usize) -> Option<Self> {
        let buf = path.get(..len)?;
        #[cfg(not(feature = "simd"))]
        let s = str::from_utf8(buf).ok()?;
        #[cfg(feature = "simd")]
        let s = simdutf8::basic::from_utf8(buf).ok()?;
        Self::new(s)
    }

    /// SAFETY: Caller ensures that `path` contains no NUL character
    pub unsafe fn new_unchecked<S: Into<Cow<'a, str>>>(reference: S) -> Self {
        Self(reference.into())
    }

    /// Join a [`RedoxReference`] with a path. Relative paths will be joined, absolute paths will
    /// be returned as-is.
    ///
    /// Returns `Some` on success and `None` if the path is not valid
    pub fn join<'b, S: Into<Cow<'b, str>>>(&self, path: S) -> Option<RedoxReference<'b>> {
        let path = path.into();
        Some(self.join_checked(RedoxReference::new(path)?))
    }

    /// Join, but checked from NUL
    pub fn join_checked<'b>(&self, path: RedoxReference<'b>) -> RedoxReference<'b> {
        if !path.is_relative() {
            // Absolute path, referenced
            path
        } else if path.0.is_empty() {
            // Empty path, return prior reference cloned
            RedoxReference(self.0.to_string().into())
        } else {
            // Relative path, append to reference
            let mut reference = self.0.clone().into_owned();
            if !reference.is_empty() && !reference.ends_with('/') {
                reference.push('/');
            }
            reference.push_str(&path.0);
            RedoxReference(reference.into())
        }
    }

    /// Canonicalize [`RedoxReference`], removing `.`, `..` and empty subpaths.
    /// In this function, relative path will be remain relative and the path remain not canon.
    ///
    /// Paths that goes upward beyond the root path will be truncated
    /// i.e. `/..` converted to `/` and `../foo/` converted to `foo`.
    pub fn canonical(self) -> Self {
        let mut canonicalized = true;
        let mut is_absolute = false;
        let mut parts = [""; PATH_MAX];
        let mut parts_len = 0;
        for (i, part) in self.0.split('/').enumerate() {
            if part.is_empty() {
                if i == 0 {
                    is_absolute = true;
                } else {
                    canonicalized = false;
                }
            } else if part == "." {
                canonicalized = false;
            } else if part == ".." {
                if parts_len > 0 {
                    parts_len -= 1;
                }
                canonicalized = false;
            } else {
                if parts_len >= PATH_MAX {
                    break;
                }
                parts[parts_len] = part;
                parts_len += 1;
            }
        }

        if canonicalized {
            return self;
        }
        let mut string = String::with_capacity(self.0.len());
        if is_absolute {
            string.push('/');
        }
        for (i, part) in parts[..parts_len].iter().enumerate() {
            if i > 0 {
                string.push('/');
            }
            string.push_str(part);
        }

        // Does not use ::new() as &self is checked from NUL
        Self(Cow::Owned(string))
    }

    /// Normalize [`RedoxReference`], removing `.`, `..` (if possible) and empty subpaths.
    /// This function will return [`None`] if the path is absolute or escapes root.
    ///
    /// This function is useful to validate if the path still remain in scope within root.
    /// To make this works within symlink, `max_upward` is provided, and also returning an integer
    /// for next `canonical_max_upward(max_upward)`. The amount of `..` will never exceed `max_upward`.
    pub fn canonical_max_upward(self, max_upward: usize) -> Option<(Self, usize)> {
        let mut canonicalized = true;
        let mut parts = [""; PATH_MAX];
        let mut parts_len = 0;
        let mut upward_depth = 0;
        for (i, part) in self.0.split('/').enumerate() {
            if part.is_empty() {
                if i == 0 {
                    // absolute path
                    return None;
                }
                canonicalized = false;
            } else if part == "." {
                canonicalized = false;
            } else if part == ".." {
                if parts_len == 0 {
                    upward_depth += 1;
                    if upward_depth > max_upward {
                        // escapes root
                        return None;
                    }
                } else {
                    parts_len -= 1;
                }

                canonicalized = false;
            } else {
                if parts_len + upward_depth >= PATH_MAX {
                    break;
                }
                parts[parts_len] = part;
                parts_len += 1;
            }
        }

        if canonicalized {
            return Some((self, max_upward + parts_len));
        }
        let mut string = String::with_capacity(self.0.len());
        for _ in 0..upward_depth {
            if !string.is_empty() {
                string.push('/');
            }
            string.push_str("..");
        }
        for part in parts[..parts_len].iter() {
            if !string.is_empty() {
                string.push('/');
            }
            string.push_str(part);
        }

        // Does not use ::new() as &self is checked from NUL
        Some((Self(string.into()), max_upward + parts_len - upward_depth))
    }

    /// Convert [`RedoxReference`] into relative path if it's not already.
    ///
    /// Converting the path to relative allows using [`Self::canonical_max_upward`]
    pub fn to_relative(self) -> Self {
        if self.is_relative() {
            return self;
        }
        match self.0 {
            Cow::Borrowed(s) => Self(s[1..].into()),
            Cow::Owned(s) => Self(s[1..].to_string().into()),
        }
    }

    /// Verify that the reference is canonicalized
    pub fn is_canon(&self) -> bool {
        (self.0.is_empty() || self.0 == "/")
            || (!self.is_relative()
                && self
                    .0
                    .split('/')
                    .skip(1)
                    .all(|seg| seg != ".." && seg != "." && seg != ""))
    }

    /// Get path upward one time
    pub fn dirname<'b>(&self) -> RedoxReference<'b> {
        self.dirname_split().0.into_owned()
    }

    /// Get path upward one time and the file name
    pub fn dirname_split(&'a self) -> (RedoxReference<'a>, Option<RedoxReference<'a>>) {
        let mut slice = &*self.0;
        let mut was_upward = false;
        loop {
            // SIMD: not eligible, filenames are often short
            let Some((dir, file)) = slice.rsplit_once('/') else {
                let file = if matches!(slice, "." | "..") {
                    None
                } else {
                    Some(RedoxReference(slice.into()))
                };
                return (RedoxReference("".into()), file);
            };
            slice = dir;
            was_upward = match file {
                "" | "." => false,
                ".." => true,
                _ if was_upward => false,
                file => {
                    return (
                        RedoxReference(slice.into()),
                        Some(RedoxReference(file.into())),
                    );
                }
            };
            if slice.is_empty() {
                return (RedoxReference("/".into()), None);
            }
        }
    }

    /// Is this path relative?
    pub fn is_relative(&self) -> bool {
        !self.0.starts_with('/')
    }

    /// Copy the content into owned
    pub fn into_owned<'b>(self) -> RedoxReference<'b> {
        RedoxReference(self.0.into_owned().into())
    }
}

impl<'a> AsRef<str> for RedoxReference<'a> {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl<'a> fmt::Display for RedoxReference<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<'a> From<RedoxReference<'a>> for Cow<'a, str> {
    fn from(value: RedoxReference<'a>) -> Self {
        value.0
    }
}

/// A fully qualified Redox path
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RedoxPath<'a> {
    /// Standard UNIX compatible format
    Standard(RedoxReference<'a>),
    /// Legacy URI format
    Legacy(RedoxScheme<'a>, RedoxReference<'a>),
}

impl<'a> RedoxPath<'a> {
    /// Create [`RedoxPath`] from absolute path
    ///
    /// Returns `Some` on success and `None` if the path contains NUL
    pub fn from_absolute<S: Into<Cow<'a, str>>>(path: S) -> Option<Self> {
        RedoxStr::new(path).and_then(|s| s.abs())
    }

    /// Create a new [`RedoxPath`] from C string, ensuring it is a valid UTF-8
    pub fn from_absolute_c<S: Into<&'a CStr>>(path: S) -> Option<Self> {
        RedoxStr::new_from_c(path).and_then(|s| s.abs())
    }

    /// Create a new [`RedoxPath`] from a buffer, ensuring it is a valid UTF-8 and valid from NUL
    pub fn from_absolute_buf(path: &'a [u8], len: usize) -> Option<Self> {
        RedoxStr::new_from_buf(path, len).and_then(|s| s.abs())
    }

    /// Create a new [`RedoxPath`] from [`RedoxReference`] if the reference is absolute
    pub fn from_reference(path: RedoxReference<'a>) -> Option<Self> {
        RedoxStr::from(path).abs()
    }

    /// Join a [`RedoxPath`] with a &str path. Relative paths will be appended to self,
    /// absolute paths will be returned as a RedoxPath, ignoring self.
    ///
    /// Returns `Some` on success and `None` if the path is not valid
    pub fn join<'b, S: Into<Cow<'b, str>>>(&self, path: S) -> Option<RedoxPath<'b>> {
        let path = path.into();
        Some(self.join_checked(RedoxReference::new(path)?))
    }

    /// Join, but checked from NUL
    pub fn join_checked<'b>(&self, path: RedoxReference<'b>) -> RedoxPath<'b> {
        if !path.is_relative() {
            // Absolute path, replaces reference
            RedoxPath::Standard(path)
        } else {
            match self {
                Self::Standard(reference) => RedoxPath::Standard(reference.join_checked(path)),
                Self::Legacy(scheme, reference) => RedoxPath::Legacy(
                    RedoxScheme(scheme.0.to_string().into()),
                    reference.join_checked(path),
                ),
            }
        }
    }

    /// Canonicalize path, see [`RedoxReference::canonical`]
    pub fn canonical(self) -> Self {
        match self {
            Self::Standard(reference) => Self::Standard(reference.canonical()),
            Self::Legacy(scheme, reference) => {
                // We cannot canonicalize legacy paths since they may need to preserve dots and
                // slashes
                Self::Legacy(scheme, reference)
            }
        }
    }

    /// Verify that the path is canonicalized.
    ///
    /// Returns false if any segment is ".", ".." or "".
    /// A path that is empty is allowed and is interpreted as "/".
    /// Legacy paths are assumed to be canonical.
    pub fn is_canon(&self) -> bool {
        match self {
            Self::Standard(reference) => reference.is_canon(),
            Self::Legacy(_scheme, _reference) => true,
        }
    }

    /// Convert into a [`RedoxScheme`] and [`RedoxReference`].
    /// - Standard paths will parse `/scheme/scheme_name/reference`, and anything not starting
    ///   with `/scheme` will be parsed as being part of the `file` scheme
    /// - Legacy paths can be instantly converted
    pub fn as_parts(&'a self) -> Option<(RedoxScheme<'a>, RedoxReference<'a>)> {
        let (scheme, path) = self.get_scheme_prefixed(true)?;
        Some((scheme, RedoxReference(path.into())))
    }

    /// Get a [`RedoxScheme`] if the reference contained `/scheme/scheme_name`.
    /// Does not fallback to default scheme if absent.
    pub fn get_scheme(&'a self) -> Option<RedoxScheme<'a>> {
        Some(self.get_scheme_prefixed(false)?.0)
    }

    fn get_scheme_prefixed(&'a self, fallback: bool) -> Option<(RedoxScheme<'a>, &'a str)> {
        const SCHEME_PREFIX_LENGTH: usize = "/scheme/".len();
        match self {
            Self::Standard(reference) => {
                // SIMD: not eligible, splitted into multiple parts and often short
                let mut parts = reference.0.split('/');
                if parts.next() == Some("") && parts.next() == Some("scheme") {
                    match parts.next() {
                        Some(scheme_name) => {
                            // Path is in /scheme/scheme_name
                            let scheme_length = SCHEME_PREFIX_LENGTH + scheme_name.len() + 1;
                            let remainder = reference.0.get(scheme_length..).unwrap_or("");

                            return Some((RedoxScheme::new(scheme_name)?, remainder));
                        }
                        None => {
                            // Path is the root scheme
                            return Some((RedoxScheme(Cow::from("")), ""));
                        }
                    }
                }
                if !fallback {
                    return None;
                }
                // If path has no special processing, it is inside the file scheme
                let remainder = reference.0.get(1..).unwrap_or("");
                return Some((RedoxScheme(Cow::from(DEFAULT_SCHEME)), remainder));
            }
            Self::Legacy(scheme, reference) => {
                // Legacy paths are already split
                Some((scheme.clone(), reference.as_ref()))
            }
        }
    }

    /// Is the scheme for this path the same as the given string?
    pub fn matches_scheme(&self, other: &str) -> bool {
        if let Some((scheme, _)) = self.get_scheme_prefixed(true) {
            scheme.0 == other
        } else {
            false
        }
    }

    /// Does the scheme match the given category, e.g. "disk-"
    pub fn is_scheme_category(&self, category: &str) -> bool {
        if let Some((scheme, _)) = self.get_scheme_prefixed(true) {
            let mut parts = scheme.0.splitn(2, '.');
            if let Some(cat) = parts.next() {
                cat == category && parts.next().is_some()
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Is this the default scheme, "/scheme/file"?
    pub fn is_default_scheme(&self) -> bool {
        self.matches_scheme(DEFAULT_SCHEME)
    }

    /// Is this a Legacy format path?
    pub fn is_legacy(&self) -> bool {
        matches!(self, RedoxPath::Legacy(_, _))
    }

    /// Format a [`RedoxPath`] into a UNIX style path
    pub fn to_standard(self) -> RedoxReference<'a> {
        match self {
            RedoxPath::Standard(reference) => reference,
            RedoxPath::Legacy(scheme, reference) => {
                RedoxReference(format!("/scheme/{}/{}", scheme.0, reference.0).into())
            }
        }
    }

    /// Format a [`RedoxPath`] into a UNIX style path,
    /// ensuring it is canonicalized
    pub fn to_standard_canon(self) -> RedoxPath<'a> {
        match self {
            RedoxPath::Standard(reference) => RedoxPath::Standard(reference.canonical()),
            RedoxPath::Legacy(scheme, reference) => {
                canonicalize_using_scheme_checked(&scheme, RedoxStr::Relative(reference))
            }
        }
    }

    /// Get path upward one time
    pub fn dirname<'b>(&self) -> RedoxPath<'b> {
        match self {
            RedoxPath::Standard(redox_reference) => RedoxPath::Standard(redox_reference.dirname()),
            RedoxPath::Legacy(redox_scheme, redox_reference) => RedoxPath::Legacy(
                RedoxScheme(redox_scheme.to_string().into()),
                redox_reference.dirname(),
            ),
        }
    }

    /// Get path upward one time and the file name
    pub fn dirname_split(&'a self) -> (RedoxPath<'a>, Option<RedoxReference<'a>>) {
        match self {
            RedoxPath::Standard(redox_reference) => {
                let (dir, name) = redox_reference.dirname_split();
                (RedoxPath::Standard(dir), name)
            }
            RedoxPath::Legacy(redox_scheme, redox_reference) => {
                let (dir, name) = redox_reference.dirname_split();
                (RedoxPath::Legacy(redox_scheme.clone(), dir), name)
            }
        }
    }

    /// Convert into [`RedoxReference`], if it an old scheme, the scheme part will be discarded.
    pub fn to_reference(self) -> RedoxReference<'a> {
        match self {
            RedoxPath::Standard(redox_reference) => redox_reference,
            RedoxPath::Legacy(_, redox_reference) => redox_reference,
        }
    }

    /// Get [`RedoxReference`], if it an old scheme, the scheme part will be discarded.
    pub fn as_reference(&self) -> &RedoxReference<'a> {
        match self {
            RedoxPath::Standard(redox_reference) => redox_reference,
            RedoxPath::Legacy(_, redox_reference) => redox_reference,
        }
    }

    /// Similar to [`canonicalize_using_cwd`]
    pub fn canonicalize_as_cwd<'b>(&self, path: RedoxStr<'b>) -> RedoxPath<'b> {
        // SAFETY: canonicalize_using_cwd_checked will None only if cwd_opt is None
        canonicalize_using_cwd_checked(Some(self), path, false).unwrap()
    }

    /// Similar to [`canonicalize_to_standard`]
    pub fn canonicalize_as_cwd_standard<'b>(&self, path: RedoxStr<'b>) -> RedoxPath<'b> {
        // SAFETY: canonicalize_to_standard_checked will None only if cwd_opt is None
        canonicalize_to_standard_checked(Some(self), path).unwrap()
    }

    /// Similar to [`canonicalize_using_cwd_with_max_upward`]
    pub fn canonicalize_as_cwd_with_max_upward<'b>(
        &self,
        relpath: RedoxReference<'b>,
        max_upward: usize,
    ) -> Option<(RedoxPath<'b>, usize)> {
        let (relpath, max_upward) = relpath.canonical_max_upward(max_upward)?;
        let newpath = self.join_checked(relpath);
        Some((newpath, max_upward))
    }

    /// Copy the content into owned
    pub fn into_owned<'b>(self) -> RedoxPath<'b> {
        match self {
            RedoxPath::Standard(redox_reference) => {
                RedoxPath::Standard(redox_reference.into_owned())
            }
            RedoxPath::Legacy(redox_scheme, redox_reference) => {
                RedoxPath::Legacy(redox_scheme.into_owned(), redox_reference.into_owned())
            }
        }
    }
}

impl<'a> fmt::Display for RedoxPath<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RedoxPath::Standard(reference) => {
                write!(f, "{}", reference.0)
            }
            RedoxPath::Legacy(scheme, reference) => {
                write!(f, "{}:{}", scheme.0, reference.0)
            }
        }
    }
}

impl<'a> From<RedoxPath<'a>> for Cow<'a, str> {
    fn from(value: RedoxPath<'a>) -> Self {
        match value {
            RedoxPath::Standard(redox_reference) => redox_reference.0,
            a => Cow::Owned(a.to_string()),
        }
    }
}

/// A valid path (checked against NUL) that can be fully qualified [`RedoxPath`] or relative [`RedoxReference`]
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RedoxStr<'a> {
    Absolute(RedoxPath<'a>),
    Relative(RedoxReference<'a>),
}

impl<'a> RedoxStr<'a> {
    /// Create a [`RedoxStr`] from a string, returns [`None`] if it contains NUL
    pub fn new<S: Into<Cow<'a, str>>>(path: S) -> Option<Self> {
        let path = path.into();
        let r = if path.starts_with('/') {
            // New /path or /scheme/scheme_name/path format, referenced
            RedoxPath::Standard(RedoxReference::new(path)?)
        } else {
            if let Some((scheme, reference)) = Self::split_scheme(&path) {
                // Old scheme_name:path format, referenced
                RedoxPath::Legacy(scheme, RedoxReference::new(reference)?)
            } else {
                // Neither an old scheme, so it's relative, referenced
                return Some(RedoxStr::Relative(RedoxReference::new(path)?));
            }
        };
        Some(RedoxStr::Absolute(r))
    }

    /// Create a [`RedoxStr`] from a buf, returns [`None`] if it not a valid UTF-8 or valid from NUL
    pub fn new_from_c<S: Into<&'a CStr>>(path: S) -> Option<Self> {
        let path = path.into();
        if matches!(path.to_bytes_with_nul().first(), Some(b'/')) {
            // New /path or /scheme/scheme_name/path format
            Some(RedoxStr::Absolute(RedoxPath::Standard(
                RedoxReference::new_from_c(path)?,
            )))
        } else {
            // Old scheme_name:path or relative format
            #[cfg(feature = "simd")]
            let s = simdutf8::basic::from_utf8(path.to_bytes()).ok()?;
            #[cfg(not(feature = "simd"))]
            let s = path.to_str().ok()?;
            // SAFETY: CStr::to_bytes() stop at NUL
            unsafe { Some(RedoxStr::new_unchecked(s)) }
        }
    }

    /// Create a [`RedoxStr`] from a buf, returns [`None`] if it not a valid UTF-8 or contains NUL
    pub fn new_from_buf(path: &'a [u8], len: usize) -> Option<RedoxStr<'a>> {
        let buf = path.get(..len)?;
        #[cfg(not(feature = "simd"))]
        let s = str::from_utf8(buf).ok()?;
        #[cfg(feature = "simd")]
        let s = simdutf8::basic::from_utf8(buf).ok()?;
        Self::new(s)
    }

    /// SAFETY: Caller ensures that `path` contains no NUL character
    pub unsafe fn new_unchecked<S: Into<Cow<'a, str>>>(path: S) -> Self {
        let path = path.into();
        let r = if path.starts_with('/') {
            // New /path or /scheme/scheme_name/path format, referenced
            RedoxPath::Standard(RedoxReference(path))
        } else {
            if let Some((scheme, reference)) = Self::split_scheme(&path) {
                // Old scheme_name:path format, referenced
                RedoxPath::Legacy(scheme, RedoxReference(reference))
            } else {
                // Neither an old scheme, so it's relative, referenced
                return RedoxStr::Relative(RedoxReference(path));
            }
        };
        RedoxStr::Absolute(r)
    }

    /// Split with scheme algorithm, retain reference if possible
    fn split_scheme(path: &Cow<'a, str>) -> Option<(RedoxScheme<'a>, Cow<'a, str>)> {
        #[cfg(not(feature = "simd"))]
        let i = {
            let mut i = 0;
            for &byte in path.as_bytes().iter() {
                if byte == b'/' {
                    return None;
                }
                if byte == b':' {
                    break;
                }
                i += 1;
            }
            i
        };
        #[cfg(feature = "simd")]
        let i = {
            let path_b = path.as_bytes();
            let chr = memchr::memchr2(b'/', b':', path_b);
            match chr {
                Some(c) if path_b.get(c).filter(|&&b| b == b':').is_some() => c,
                _ => return None,
            }
        };
        let (scheme, path): (Cow<'a, str>, Cow<'a, str>) = match path {
            Cow::Borrowed(s) => (s.get(..i)?.into(), s.get(i + 1..)?.into()),
            Cow::Owned(s) => (
                s.get(..i)?.to_string().into(),
                s.get(i + 1..)?.to_string().into(),
            ),
        };
        Some((RedoxScheme::new(scheme)?, path))
    }

    /// Get the reference to absolute path
    pub fn as_abs(&'a self) -> Option<&'a RedoxPath<'a>> {
        match self {
            RedoxStr::Absolute(redox_path) => Some(redox_path),
            _ => None,
        }
    }
    /// Get the reference to relative path
    pub fn as_rel(&'a self) -> Option<&'a RedoxReference<'a>> {
        match self {
            RedoxStr::Relative(redox_reference) => Some(redox_reference),
            _ => None,
        }
    }
    /// Get the absolute path
    pub fn abs(self) -> Option<RedoxPath<'a>> {
        match self {
            RedoxStr::Absolute(redox_path) => Some(redox_path),
            _ => None,
        }
    }
    /// Get the relative path
    pub fn rel(self) -> Option<RedoxReference<'a>> {
        match self {
            RedoxStr::Relative(redox_reference) => Some(redox_reference),
            _ => None,
        }
    }
    /// Is this path canonicalized?
    pub fn is_canon(&self) -> bool {
        match self {
            RedoxStr::Absolute(redox_path) => redox_path.is_canon(),
            RedoxStr::Relative(redox_reference) => redox_reference.is_canon(),
        }
    }
    /// Convert into canonicalized path
    pub fn canonical(self) -> Self {
        match self {
            RedoxStr::Absolute(redox_path) => RedoxStr::from(redox_path.canonical()),
            RedoxStr::Relative(redox_reference) => RedoxStr::from(redox_reference.canonical()),
        }
    }
    /// Convert into standard UNIX style path
    pub fn to_standard(self) -> RedoxReference<'a> {
        match self {
            RedoxStr::Absolute(redox_path) => redox_path.to_standard(),
            RedoxStr::Relative(redox_reference) => redox_reference,
        }
    }
    /// Get path upward one time
    pub fn dirname<'b>(&self) -> RedoxStr<'b> {
        match self {
            RedoxStr::Absolute(redox_path) => RedoxStr::from(redox_path.dirname()),
            RedoxStr::Relative(redox_reference) => RedoxStr::from(redox_reference.dirname()),
        }
    }
    /// Get path upward one time and the file name
    pub fn dirname_split(&'a self) -> (RedoxStr<'a>, Option<RedoxReference<'a>>) {
        match self {
            RedoxStr::Absolute(redox_path) => {
                let (dir, name) = redox_path.dirname_split();
                (RedoxStr::from(dir), name)
            }
            RedoxStr::Relative(redox_reference) => {
                let (dir, name) = redox_reference.dirname_split();
                (RedoxStr::from(dir), name)
            }
        }
    }
    /// This is path empty?
    pub fn is_empty(&self) -> bool {
        match self {
            RedoxStr::Absolute(_) => false, // Absolute path always have `/` for POSIX path or `:` for legacy path
            RedoxStr::Relative(redox_reference) => redox_reference.as_ref().is_empty(),
        }
    }
    /// Join two path
    pub fn join<'b, S: Into<Cow<'b, str>>>(&self, path: S) -> Option<RedoxStr<'b>> {
        match self {
            RedoxStr::Absolute(redox_path) => Some(RedoxStr::from(redox_path.join(path)?)),
            RedoxStr::Relative(redox_reference) => {
                Some(RedoxStr::from(redox_reference.join(path)?))
            }
        }
    }
    /// Join two path
    pub fn join_checked<'b>(&self, path: RedoxReference<'b>) -> RedoxStr<'b> {
        match self {
            RedoxStr::Absolute(redox_path) => RedoxStr::from(redox_path.join_checked(path)),
            RedoxStr::Relative(redox_reference) => {
                RedoxStr::from(redox_reference.join_checked(path))
            }
        }
    }
    /// Is this path relative?
    pub fn is_relative(&self) -> bool {
        // Does no need to check RedoxReference::is_relative because it's checked from From or new function
        matches!(self, RedoxStr::Relative(_))
    }

    /// Copy the content into owned
    pub fn into_owned<'b>(self) -> RedoxStr<'b> {
        match self {
            RedoxStr::Absolute(redox_path) => RedoxStr::Absolute(redox_path.into_owned()),
            RedoxStr::Relative(redox_reference) => RedoxStr::Relative(redox_reference.into_owned()),
        }
    }
}

impl<'a> From<RedoxPath<'a>> for RedoxStr<'a> {
    fn from(value: RedoxPath<'a>) -> Self {
        Self::Absolute(value)
    }
}

impl<'a> From<RedoxReference<'a>> for RedoxStr<'a> {
    fn from(value: RedoxReference<'a>) -> Self {
        // try to check if it absolute
        if !value.is_relative() {
            RedoxStr::Absolute(RedoxPath::Standard(value))
        } else {
            RedoxStr::Relative(value)
        }
    }
}
impl<'a> From<RedoxStr<'a>> for Cow<'a, str> {
    fn from(value: RedoxStr<'a>) -> Self {
        match value {
            RedoxStr::Absolute(redox_path) => redox_path.into(),
            RedoxStr::Relative(redox_reference) => redox_reference.into(),
        }
    }
}

/// Make a relative path absolute using an optional current working directory.
///
/// Given a cwd of "/scheme/scheme_name/dir_name", this function will turn
/// path "foo" into /scheme/scheme_name/dir_name/foo".
/// "/foo" will be left as is, because it is already absolute.
/// "." and empty segments "//" will be removed.
/// ".." will be resolved by backing up one directory, except at the root,
/// where ".." will be ignored and removed.
///
/// For old format schemes,
/// given a cwd of "scheme:/path", this function will turn "foo" into "scheme:/path/foo".
/// "/foo" will turn into "file:/foo". "bar:/foo" will be used directly, as it is already
/// absolute.
pub fn canonicalize_using_cwd<'a>(cwd_opt: Option<&'a str>, path: &'a str) -> Option<Cow<'a, str>> {
    let cwd_opt = cwd_opt.and_then(|s| RedoxPath::from_absolute(Cow::Borrowed(s)));
    let absolute = canonicalize_using_cwd_checked(cwd_opt.as_ref(), RedoxStr::new(path)?, false)?;
    Some(absolute.into())
}

/// `canonicalize_using_cwd` but for internal use.
/// `in_scheme_root`: Treat cwd_opt as the root of scheme, similar to
///  openat RESOLVE_IN_ROOT but still allowing access to other scheme root.
fn canonicalize_using_cwd_checked<'a, 'b>(
    cwd_opt: Option<&RedoxPath<'a>>,
    path: RedoxStr<'b>,
    in_scheme_root: bool,
) -> Option<RedoxPath<'b>> {
    let absolute = match (path, in_scheme_root) {
        (RedoxStr::Absolute(absolute), true) if absolute.get_scheme().is_some() => absolute,
        (RedoxStr::Absolute(absolute), true) => {
            cwd_opt?.join_checked(absolute.to_standard().to_relative())
        }
        (RedoxStr::Absolute(absolute), false) => absolute,
        (RedoxStr::Relative(relative), true) => cwd_opt?.join_checked(relative.canonical()),
        (RedoxStr::Relative(relative), false) => cwd_opt?.join_checked(relative),
    };
    Some(absolute.canonical())
}

/// Canonicalize as in [`canonicalize_using_cwd`], but Legacy format paths
/// are canonicalized into UNIX format.
pub fn canonicalize_to_standard<'a>(
    cwd_opt: Option<&'a str>,
    path: &'a str,
) -> Option<Cow<'a, str>> {
    let absolute = match RedoxPath::from_absolute(path) {
        Some(absolute) => absolute,
        None => {
            let absolute = RedoxPath::from_absolute(cwd_opt?)?;
            absolute.join(path)?
        }
    };
    Some(absolute.to_standard_canon().into())
}

/// [`canonicalize_to_standard`] but for internal use.
fn canonicalize_to_standard_checked<'a, 'b>(
    cwd_opt: Option<&RedoxPath<'a>>,
    path: RedoxStr<'b>,
) -> Option<RedoxPath<'b>> {
    let path = match path {
        RedoxStr::Absolute(absolute) => absolute,
        RedoxStr::Relative(relative) => cwd_opt?.join_checked(relative),
    }
    .to_standard_canon();
    Some(path)
}

/// Make a path that is relative to the root of a scheme into a full path,
/// following the rules of [`canonicalize_using_cwd`].
///
/// Returns the Some if a valid path can be constructed,
/// None if the scheme name is not valid or some other error occurs.
/// The returned value is guaranteed to be UNIX, but not guaranteed to be equal with given scheme.
pub fn canonicalize_using_scheme<'a>(scheme: &'a str, path: &'a str) -> Option<Cow<'a, str>> {
    let path = RedoxStr::new(path)?;
    Some(canonicalize_using_scheme_checked(&RedoxScheme::new(scheme)?, path).into())
}

/// [`canonicalize_using_scheme`] but for internal use.
fn canonicalize_using_scheme_checked<'a, 'b>(
    scheme: &RedoxScheme<'_>,
    path: RedoxStr<'b>,
) -> RedoxPath<'b> {
    let path = match path {
        RedoxStr::Absolute(s) => {
            if !s.is_legacy() && s.matches_scheme(&scheme.0) {
                // Canonicalized and being in the same scheme, return early
                // to avoid allocation from scheme_path_checked
                return s;
            }
            RedoxStr::Absolute(s)
        }
        path => path,
    };
    // SAFETY: canonicalize_using_cwd_checked will only None if cwd_opt is None
    canonicalize_using_cwd_checked(Some(&scheme_path_checked(&scheme)), path, true).unwrap()
}

/// Make a path that is relative to the root of a scheme into a full path,
/// following the rules of [`canonicalize_using_cwd`], but with upward_max to ensure
/// the returned path stays within the true root.
pub fn canonicalize_using_cwd_with_max_upward<'a>(
    cwd: &str,
    relpath: &'a str,
    max_upward: usize,
) -> Option<(Cow<'a, str>, usize)> {
    // Using RedoxStr so cwd using relative path (as in scheme) is also accepted
    let cwd = RedoxStr::new(Cow::Borrowed(cwd))?;
    let (absolute, max_upward) = canonicalize_using_cwd_with_max_upward_checked(
        &cwd,
        RedoxStr::new(relpath)?.rel()?,
        max_upward,
    )?;
    Some((absolute.into(), max_upward))
}

/// [`canonicalize_using_cwd_with_max_upward`] but for internal use.
fn canonicalize_using_cwd_with_max_upward_checked<'a, 'b>(
    cwd: &RedoxStr<'a>,
    relpath: RedoxReference<'b>,
    max_upward: usize,
) -> Option<(RedoxStr<'b>, usize)> {
    let (relpath, max_upward) = relpath.canonical_max_upward(max_upward)?;
    let relnew = cwd.join_checked(relpath);
    Some((relnew, max_upward))
}

/// The standard path for a given scheme name
///
/// Returns Some if the scheme name is valid, None otherwise
pub fn scheme_path(name: &str) -> Option<RedoxPath<'_>> {
    let scheme = RedoxScheme::new(name)?;
    Some(scheme_path_checked(&scheme))
}

/// [`scheme_path`] but for internal use.
fn scheme_path_checked<'a, 'b>(scheme: &'a RedoxScheme<'a>) -> RedoxPath<'b> {
    if scheme.0.is_empty() {
        return RedoxPath::Standard(RedoxReference("/scheme".into()));
    }
    RedoxPath::Standard(RedoxReference(format!("/scheme/{}", scheme.0).into()))
}

/// Make a scheme name (not a full path) for a device of a given category
///
/// Returns Some if the resulting scheme name is valid, None otherwise
pub fn make_scheme_name<'a, 'b>(category: &'a str, detail: &'a str) -> Option<RedoxScheme<'b>> {
    let name = format!("{}.{}", category, detail);
    RedoxScheme::new(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    // Tests absolute paths without scheme
    #[test]
    fn test_absolute() {
        let cwd_opt = None;
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/"),
            Some(Cow::Borrowed("/"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/file"),
            Some(Cow::Borrowed("/file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/folder/file"),
            Some(Cow::Borrowed("/folder/file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/folder/file/"),
            Some(Cow::Borrowed("/folder/file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/folder/file//"),
            Some(Cow::Borrowed("/folder/file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/folder/../file"),
            Some(Cow::Borrowed("/file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/folder/../.."),
            Some(Cow::Borrowed("/"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/folder/../../../.."),
            Some(Cow::Borrowed("/"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/.."),
            Some(Cow::Borrowed("/"))
        );
    }

    // Test relative paths using new scheme
    #[test]
    fn test_new_relative() {
        let cwd_opt = Some("/scheme/foo");
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "file"),
            Some(Cow::Borrowed("/scheme/foo/file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "folder/file"),
            Some(Cow::Borrowed("/scheme/foo/folder/file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "folder/../file"),
            Some(Cow::Borrowed("/scheme/foo/file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "folder/../.."),
            Some(Cow::Borrowed("/scheme"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "folder/../../../.."),
            Some(Cow::Borrowed("/"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, ".."),
            Some(Cow::Borrowed("/scheme"))
        );
    }

    // Tests paths prefixed with /scheme/
    #[test]
    fn test_new_scheme() {
        let cwd_opt = None;
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/scheme/bar/"),
            Some(Cow::Borrowed("/scheme/bar"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/scheme/bar/file"),
            Some(Cow::Borrowed("/scheme/bar/file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/scheme/bar/folder/file"),
            Some(Cow::Borrowed("/scheme/bar/folder/file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/scheme/bar/folder/../file"),
            Some(Cow::Borrowed("/scheme/bar/file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/scheme/bar/folder/../.."),
            Some(Cow::Borrowed("/scheme"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/scheme/bar/folder/../../../.."),
            Some(Cow::Borrowed("/"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "/scheme/bar/.."),
            Some(Cow::Borrowed("/scheme"))
        );

        assert_eq!(
            canonicalize_using_scheme("bar", ""),
            Some(Cow::Borrowed("/scheme/bar"))
        );
        assert_eq!(
            canonicalize_using_scheme("bar", "foo"),
            Some(Cow::Borrowed("/scheme/bar/foo"))
        );
        assert_eq!(
            canonicalize_using_scheme("foo", "/bar"),
            Some(Cow::Borrowed("/scheme/foo/bar"))
        );
        assert_eq!(
            canonicalize_using_scheme("bar", ".."),
            Some(Cow::Borrowed("/scheme/bar"))
        );
        assert_eq!(
            canonicalize_using_scheme("file", "/scheme/foo/bar"),
            Some(Cow::Borrowed("/scheme/foo/bar"))
        );
    }

    // Test relative paths using old scheme
    #[test]
    fn test_old_relative() {
        let cwd_opt = Some("foo:");
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "file"),
            Some(Cow::Borrowed("foo:file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "folder/file"),
            Some(Cow::Borrowed("foo:folder/file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "folder/../file"),
            Some(Cow::Borrowed("foo:folder/../file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "folder/../.."),
            Some(Cow::Borrowed("foo:folder/../.."))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "folder/../../../.."),
            Some(Cow::Borrowed("foo:folder/../../../.."))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, ".."),
            Some(Cow::Borrowed("foo:.."))
        );
    }

    // Tests paths prefixed with scheme_name:
    #[test]
    fn test_old_scheme() {
        let cwd_opt = None;
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "bar:"),
            Some(Cow::Borrowed("bar:"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "bar:file"),
            Some(Cow::Borrowed("bar:file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "bar:folder/file"),
            Some(Cow::Borrowed("bar:folder/file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "bar:folder/../file"),
            Some(Cow::Borrowed("bar:folder/../file"))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "bar:folder/../.."),
            Some(Cow::Borrowed("bar:folder/../.."))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "bar:folder/../../../.."),
            Some(Cow::Borrowed("bar:folder/../../../.."))
        );
        assert_eq!(
            canonicalize_using_cwd(cwd_opt, "bar:.."),
            Some(Cow::Borrowed("bar:.."))
        );
    }

    // Tests max upward for symlink
    #[test]
    fn test_max_upward() {
        // new format
        assert_eq!(
            canonicalize_using_cwd_with_max_upward("/", "foo/bar", 0),
            Some((Cow::Borrowed("/foo/bar"), 2))
        );
        assert_eq!(
            canonicalize_using_cwd_with_max_upward("/foo/", "bar/../baz/qux", 0),
            Some((Cow::Borrowed("/foo/baz/qux"), 2))
        );
        assert_eq!(
            canonicalize_using_cwd_with_max_upward("foo/../bar", "baz/./../qux", 0),
            Some((Cow::Borrowed("foo/../bar/qux"), 1))
        );
        assert_eq!(
            canonicalize_using_cwd_with_max_upward("/foo", "../bar", 1),
            Some((Cow::Borrowed("/foo/../bar"), 1))
        );
        assert_eq!(
            canonicalize_using_cwd_with_max_upward("foo/..", "../bar/..", 2),
            Some((Cow::Borrowed("foo/../.."), 1))
        );
        // old format
        assert_eq!(
            canonicalize_using_cwd_with_max_upward("foo:", "bar/baz", 0),
            Some((Cow::Borrowed("foo:bar/baz"), 2))
        );
        assert_eq!(
            canonicalize_using_cwd_with_max_upward("foo:", "bar/baz//../../", 0),
            Some((Cow::Borrowed("foo:"), 0))
        );
        assert_eq!(
            canonicalize_using_cwd_with_max_upward("foo:/bar/..", "baz/./qux/..", 0),
            Some((Cow::Borrowed("foo:/bar/../baz"), 1))
        );
        // neg tests
        assert_eq!(
            canonicalize_using_cwd_with_max_upward("/foo", "../bar", 0),
            None
        );
        assert_eq!(
            canonicalize_using_cwd_with_max_upward("/foo", "/bar", 0),
            None
        );
        assert_eq!(
            canonicalize_using_cwd_with_max_upward("/foo/..", "../bar/..", 0),
            None
        );
    }
    // Test whether paths are borrowed or owned
    #[test]
    fn test_cow_borrowing() {
        let cwd_opt = None;
        for (path, should_owned) in [
            ("/foo/bar/baz", false),
            ("/scheme/foo/bar/baz", false),
            ("foo/bar/baz", false), // this is None(Cow::Borrowed)
            ("/scheme/foo/../baz", true),
            ("foo:bar", true),
        ] {
            assert_eq!(
                matches!(canonicalize_using_cwd(cwd_opt, path), Some(Cow::Owned(_))),
                should_owned
            );
        }
    }
    // Test whether dirname works
    #[test]
    fn test_dirname() {
        for (a, b) in [
            ("/foo/bar/baz", "/foo/bar"),
            ("/baz/foo/bar/", "/baz/foo"),
            ("/", "/"),
            ("/scheme/foo", "/scheme"),
            ("foo:bar/baz/", "foo:bar"),
            ("foo:bar", "foo:"),
            ("foo:", "foo:"),
        ] {
            assert_eq!(
                RedoxPath::from_absolute(a).unwrap().dirname(),
                RedoxPath::from_absolute(b).unwrap()
            );
        }
        for (a, b) in [
            ("foo/bar/.", "foo"),
            ("./foo/bar/..", "."),
            ("./.", ""),
            (".", ""),
            ("", ""),
        ] {
            assert_eq!(
                RedoxStr::new(a).unwrap().dirname(),
                RedoxStr::new(b).unwrap()
            );
        }
    }

    // Tests paths that may be used with orbital:
    #[test]
    fn test_orbital_scheme() {
        for flag_str in &["", "abflrtu"] {
            for x in &[-1, 0, 1] {
                for y in &[-1, 0, 1] {
                    for w in &[0, 1] {
                        for h in &[0, 1] {
                            for title in &[
                                "",
                                "title",
                                "title/with/slashes",
                                "title:with:colons",
                                "title/../with/../dots/..",
                            ] {
                                let path = format!(
                                    "orbital:{}/{}/{}/{}/{}/{}",
                                    flag_str, x, y, w, h, title
                                );
                                assert_eq!(
                                    canonicalize_using_cwd(None, &path),
                                    Some((&path).into())
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // Tests path splitting to parts
    #[test]
    fn test_parts() {
        for (path, scheme, reference) in [
            ("/foo/bar/baz", "file", "foo/bar/baz"),
            ("/scheme/foo/bar/baz", "foo", "bar/baz"),
            ("/", "file", ""),
            ("/bar", "file", "bar"),
            ("/...", "file", "..."),
            ("//double/slash", "file", "/double/slash"),
            ("/ending/in/slash/", "file", "ending/in/slash/"),
            ("/contains/dot/.", "file", "contains/dot/."),
            ("/contains/dotdot/..", "file", "contains/dotdot/.."),
        ] {
            let redox_path = RedoxPath::from_absolute(path).unwrap();
            let parts = redox_path.as_parts();
            assert_eq!(
                (path, parts),
                (
                    path,
                    Some((
                        RedoxScheme::new(scheme).unwrap(),
                        RedoxReference::new(reference).unwrap()
                    ))
                )
            );
            if path.starts_with("/scheme") {
                let to_string = format!("/scheme/{scheme}");
                let joined_path = RedoxPath::from_absolute(&to_string)
                    .unwrap()
                    .join(reference)
                    .unwrap();
                assert_eq!(path, &format!("{joined_path}"));
            } else {
                assert_eq!(path, &format!("/{reference}"));
            }
        }

        // fail if the path is not absolute
        assert_eq!(RedoxPath::from_absolute("not/absolute"), None);

        // fail if the scheme characters are not valid
        for path in ["/scheme/use:colon/", "/scheme/βeta/"] {
            let redox_path = RedoxPath::from_absolute(path).unwrap();
            let parts = redox_path.as_parts();
            assert_eq!((path, parts), (path, None));
        }
    }

    #[test]
    fn test_old_scheme_parts() {
        for (path, scheme, reference) in [
            ("foo:bar/baz", "foo", "bar/baz"),
            ("emptyref:", "emptyref", ""),
            (":emptyscheme", "", "emptyscheme"),
        ] {
            let redox_path = RedoxPath::from_absolute(path).unwrap();
            let parts = redox_path.as_parts();
            assert_eq!(
                (path, parts),
                (
                    path,
                    Some((
                        RedoxScheme::new(scheme).unwrap(),
                        RedoxReference::new(reference).unwrap()
                    ))
                )
            );
        }

        // slash is not allowed in scheme names
        assert_eq!(RedoxPath::from_absolute("scheme/withslash:path"), None);
        // empty path is not allowed for from_absolute
        assert_eq!(RedoxPath::from_absolute(""), None)
    }

    #[test]
    fn test_matches() {
        assert!(RedoxPath::from_absolute("/scheme/foo")
            .unwrap()
            .matches_scheme("foo"));
        assert!(RedoxPath::from_absolute("/scheme/foo/bar")
            .unwrap()
            .matches_scheme("foo"));
        assert!(!RedoxPath::from_absolute("/scheme/foo")
            .unwrap()
            .matches_scheme("bar"));
        assert!(RedoxPath::from_absolute("foo:")
            .unwrap()
            .matches_scheme("foo"));
        assert!(RedoxPath::from_absolute(
            canonicalize_using_cwd(Some("/scheme/foo"), "bar").unwrap()
        )
        .unwrap()
        .matches_scheme("foo"));
        assert!(
            RedoxPath::from_absolute(canonicalize_using_cwd(Some("/foo"), "bar").unwrap())
                .unwrap()
                .matches_scheme("file")
        );
        assert!(RedoxPath::from_absolute(
            canonicalize_using_cwd(Some("/scheme"), "foo/bar").unwrap()
        )
        .unwrap()
        .matches_scheme("foo"));
        assert!(RedoxPath::from_absolute("foo:/bar")
            .unwrap()
            .matches_scheme("foo"));
        assert!(!RedoxPath::from_absolute("foo:/bar")
            .unwrap()
            .matches_scheme("bar"));
        assert!(RedoxPath::from_absolute("/scheme/file")
            .unwrap()
            .is_default_scheme());
        assert!(!RedoxPath::from_absolute("/scheme/foo")
            .unwrap()
            .is_default_scheme());
        assert!(RedoxPath::from_absolute("file:bar")
            .unwrap()
            .is_default_scheme());
        assert!(RedoxPath::from_absolute("file:")
            .unwrap()
            .is_default_scheme());
        assert!(!RedoxPath::from_absolute("foo:bar")
            .unwrap()
            .is_default_scheme());
        assert!(RedoxPath::from_absolute("foo:bar").unwrap().is_legacy());
        assert!(!RedoxPath::from_absolute("/foo/bar").unwrap().is_legacy());
    }

    #[test]
    fn test_to_standard() {
        assert_eq!(
            RedoxPath::from_absolute("foo:bar")
                .unwrap()
                .to_standard()
                .as_ref(),
            "/scheme/foo/bar"
        );
        assert_eq!(
            RedoxPath::from_absolute("file:bar")
                .unwrap()
                .to_standard()
                .as_ref(),
            "/scheme/file/bar"
        );
        assert_eq!(
            RedoxPath::from_absolute("/scheme/foo/bar")
                .unwrap()
                .to_standard()
                .as_ref(),
            "/scheme/foo/bar"
        );
        assert_eq!(
            RedoxPath::from_absolute("/foo/bar")
                .unwrap()
                .to_standard()
                .as_ref(),
            "/foo/bar"
        );
        assert_eq!(
            &RedoxPath::from_absolute("foo:bar/../bar2")
                .unwrap()
                .to_standard_canon()
                .to_string(),
            "/scheme/foo/bar2"
        );
        assert_eq!(
            &RedoxPath::from_absolute("file:bar/./../bar2")
                .unwrap()
                .to_standard_canon()
                .to_string(),
            "/scheme/file/bar2"
        );
        assert_eq!(
            &RedoxPath::from_absolute("/scheme/file/bar/./../../foo/bar")
                .unwrap()
                .to_standard_canon()
                .to_string(),
            "/scheme/foo/bar"
        );
        assert_eq!(
            &RedoxPath::from_absolute("/foo/bar")
                .unwrap()
                .to_standard_canon()
                .to_string(),
            "/foo/bar"
        );
        assert_eq!(
            &canonicalize_to_standard(None, "/scheme/foo/bar").unwrap(),
            "/scheme/foo/bar"
        );
        assert_eq!(
            &canonicalize_to_standard(None, "foo:bar").unwrap(),
            "/scheme/foo/bar"
        );
        assert_eq!(
            &canonicalize_to_standard(None, "foo:bar/../..").unwrap(),
            "/scheme/foo"
        );
        assert_eq!(
            &canonicalize_to_standard(None, "/scheme/foo/bar/..").unwrap(),
            "/scheme/foo"
        );
        assert_eq!(
            &canonicalize_to_standard(None, "foo:bar/bar2/..").unwrap(),
            "/scheme/foo/bar"
        );
    }

    #[test]
    fn test_scheme_path() {
        assert_eq!(
            scheme_path("foo"),
            Some(RedoxPath::from_absolute("/scheme/foo").unwrap())
        );
        assert_eq!(
            scheme_path(""),
            Some(RedoxPath::from_absolute("/scheme").unwrap())
        );
        assert_eq!(scheme_path("/foo"), None);
        assert_eq!(scheme_path("foo/bar"), None);
        assert_eq!(scheme_path("foo:"), None);
    }

    #[test]
    fn test_category() {
        assert_eq!(
            make_scheme_name("foo", "bar"),
            Some(RedoxScheme::new("foo.bar").unwrap())
        );
        assert_eq!(
            RedoxPath::from_absolute(
                scheme_path(make_scheme_name("foo", "bar").unwrap().as_ref()).unwrap()
            )
            .unwrap(),
            RedoxPath::Standard(RedoxReference::new("/scheme/foo.bar").unwrap())
        );
        assert_eq!(make_scheme_name("foo", "/bar"), None);
        assert_eq!(make_scheme_name("foo", ":bar"), None);
        assert!(RedoxPath::from_absolute(
            scheme_path(make_scheme_name("foo", "bar").unwrap().as_ref()).unwrap()
        )
        .unwrap()
        .is_scheme_category("foo"));
        assert!(RedoxPath::from_absolute("/scheme/foo.bar/bar2")
            .unwrap()
            .is_scheme_category("foo"));
        assert!(!RedoxPath::from_absolute("/scheme/foo/bar")
            .unwrap()
            .is_scheme_category("foo"));
        assert!(!RedoxPath::from_absolute("/foo.bar/bar2")
            .unwrap()
            .is_scheme_category("foo"));
    }
}
