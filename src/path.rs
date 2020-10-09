use crate::{
    pack::{Pack, PackError, varint_len, encode_varint, decode_varint},
    chars::Chars,
    utils,
};
use std::{
    borrow::Borrow,
    borrow::Cow,
    cmp::{Eq, Ord, PartialEq, PartialOrd},
    convert::{AsRef, From},
    fmt,
    iter::Iterator,
    ops::Deref,
    result::Result,
    str::{self, FromStr},
    sync::Arc,
};
use bytes::Buf;

pub static ESC: char = '\\';
pub static SEP: char = '/';

/// A path in the namespace. Paths are immutable and reference
/// counted.  Path components are seperated by /, which may be escaped
/// with \. / and \ are the only special characters in path, any other
/// unicode character may be used. Path lengths are not limited on the
/// local machine, but may be restricted by maximum message size on
/// the wire.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Path(Arc<str>);

impl Pack for Path {
    fn len(&self) -> usize {
        varint_len(self.0.len() as u64) + self.0.len()
    }

    fn encode(&self, buf: &mut bytes::BytesMut) -> Result<(), PackError> {
        encode_varint(self.0.len() as u64, buf);
        Ok(buf.extend_from_slice(self.0.as_bytes()))
    }

    fn decode(buf: &mut bytes::BytesMut) -> Result<Self, PackError> {
        let len = decode_varint(buf)?;
        if len as usize > buf.remaining() {
            Err(PackError::TooBig)
        } else {
            let bytes = buf.split_to(len as usize);
            match str::from_utf8(&*bytes) {
                Err(_) => Err(PackError::InvalidFormat),
                Ok(s) => Ok(Path(Arc::from(s)))
            }            
        }
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for Path {
    fn as_ref(&self) -> &str {
        &*self.0
    }
}

impl Borrow<str> for Path {
    fn borrow(&self) -> &str {
        &*self.0
    }
}

impl Deref for Path {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Chars> for Path {
    fn from(c: Chars) -> Path {
        if is_canonical(&c) {
            Path(Arc::from(c.as_ref()))
        } else {
            Path(Arc::from(canonize(&c)))
        }
    }
}

impl From<String> for Path {
    fn from(s: String) -> Path {
        if is_canonical(&s) {
            Path(Arc::from(s))
        } else {
            Path(Arc::from(canonize(&s)))
        }
    }
}

impl From<&'static str> for Path {
    fn from(s: &'static str) -> Path {
        if is_canonical(s) {
            Path(Arc::from(s))
        } else {
            Path(Arc::from(canonize(s)))
        }
    }
}

impl<'a> From<&'a String> for Path {
    fn from(s: &String) -> Path {
        if is_canonical(s.as_str()) {
            Path(Arc::from(s.clone()))
        } else {
            Path(Arc::from(canonize(s.as_str())))
        }
    }
}

impl FromStr for Path {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Path(Arc::from(s)))
    }
}

fn is_canonical(s: &str) -> bool {
    for _ in Path::parts(s).filter(|p| *p == "") {
        return false;
    }
    true
}

fn canonize(s: &str) -> String {
    let mut res = String::with_capacity(s.len());
    if s.len() > 0 {
        if s.starts_with(SEP) {
            res.push(SEP)
        }
        let mut first = true;
        for p in Path::parts(s).filter(|p| *p != "") {
            if first {
                first = false;
            } else {
                res.push(SEP)
            }
            res.push_str(p);
        }
    }
    res
}

enum DirNames<'a> {
    Root(bool),
    Path { cur: &'a str, all: &'a str, base: usize },
}

impl<'a> Iterator for DirNames<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            DirNames::Root(false) => None,
            DirNames::Root(true) => {
                *self = DirNames::Root(false);
                Some("/")
            }
            DirNames::Path { ref mut cur, ref all, ref mut base } => {
                if *base == all.len() {
                    None
                } else {
                    match Path::find_sep(cur) {
                        None => {
                            *base = all.len();
                            Some(all)
                        }
                        Some(p) => {
                            *base += p + 1;
                            *cur = &all[*base..];
                            Some(&all[0..*base - 1])
                        }
                    }
                }
            }
        }
    }
}

impl Path {
    /// returns /
    pub fn root() -> Path {
        // CR estokes: need a good solution for using SEP here
        Path::from("/")
    }

    /// returns true if the path starts with /, false otherwise
    pub fn is_absolute<T: AsRef<str> + ?Sized>(p: &T) -> bool {
        p.as_ref().starts_with(SEP)
    }

    /// This will escape the path seperator and the escape character
    /// in a path part. If you want to be sure that e.g. `append` will
    /// only append 1 level, then you should call this function on
    /// your part before appending it.
    ///
    /// # Examples
    /// ```
    /// use netidx::path::Path;
    /// assert_eq!("foo\\/bar", &*Path::escape("foo/bar"));
    /// assert_eq!("\\\\hello world", &*Path::escape("\\hello world"));
    /// ```
    pub fn escape<T: AsRef<str> + ?Sized>(s: &T) -> Cow<str> {
        utils::escape(s, ESC, SEP)
    }

    /// This will unescape the path seperator and the escape character
    /// in a path part.
    ///
    /// # Examples
    /// ```
    /// use netidx::path::Path;
    /// assert_eq!("foo/bar", &*Path::unescape("foo\\/bar"));
    /// assert_eq!("\\hello world", &*Path::unescape("\\\\hello world"));
    /// ```
    pub fn unescape<T: AsRef<str> + ?Sized>(s: &T) -> Cow<str> {
        let s = s.as_ref();
        if !(0..s.len()).into_iter().any(|i| utils::is_escaped(s, SEP, ESC, i)) {
            Cow::Borrowed(s)
        } else {
            let mut out = String::with_capacity(s.len());
            for (i, c) in s.chars().enumerate() {
                if utils::is_escaped(s, SEP, ESC, i) {
                    out.pop();
                }
                out.push(c);
            }
            Cow::Owned(out)
        }
    }

    /// return a new path with the specified string appended as a new
    /// part separated by the pathsep char.
    ///
    /// # Examples
    /// ```
    /// use netidx::path::Path;
    /// let p = Path::root().append("bar").append("baz");
    /// assert_eq!(&*p, "/bar/baz");
    ///
    /// let p = Path::root().append("/bar").append("//baz//////foo/");
    /// assert_eq!(&*p, "/bar/baz/foo");
    /// ```
    pub fn append<T: AsRef<str> + ?Sized>(&self, other: &T) -> Self {
        let other = other.as_ref();
        if other.len() == 0 {
            self.clone()
        } else {
            let mut res = String::with_capacity(self.as_ref().len() + other.len());
            res.push_str(self.as_ref());
            res.push(SEP);
            res.push_str(other);
            Path::from(res)
        }
    }

    /// return an iterator over the parts of the path. The path
    /// separator may be escaped with \. and a literal \ may be
    /// represented as \\.
    ///
    /// # Examples
    /// ```
    /// use netidx::path::Path;
    /// let p = Path::from("/foo/bar/baz");
    /// assert_eq!(Path::parts(&p).collect::<Vec<_>>(), vec!["foo", "bar", "baz"]);
    ///
    /// let p = Path::from(r"/foo\/bar/baz");
    /// assert_eq!(Path::parts(&p).collect::<Vec<_>>(), vec![r"foo\/bar", "baz"]);
    ///
    /// let p = Path::from(r"/foo\\/bar/baz");
    /// assert_eq!(Path::parts(&p).collect::<Vec<_>>(), vec![r"foo\\", "bar", "baz"]);
    ///
    /// let p = Path::from(r"/foo\\\/bar/baz");
    /// assert_eq!(Path::parts(&p).collect::<Vec<_>>(), vec![r"foo\\\/bar", "baz"]);
    /// ```
    pub fn parts<T: AsRef<str> + ?Sized>(s: &T) -> impl Iterator<Item = &str> {
        let s = s.as_ref();
        let skip = if s == "/" {
            2
        } else if s.starts_with("/") {
            1
        } else {
            0
        };
        utils::split_escaped(s, ESC, SEP).skip(skip)
    }

    /// Return an iterator over all the dirnames in the path starting
    /// from the root and ending with the entire path.
    ///
    /// # Examples
    /// ```
    /// use netidx::path::Path;
    /// let p = Path::from("/some/path/ending/in/foo");
    /// let mut bn = Path::dirnames(&p);
    /// assert_eq!(bn.next(), Some("/"));
    /// assert_eq!(bn.next(), Some("/some"));
    /// assert_eq!(bn.next(), Some("/some/path"));
    /// assert_eq!(bn.next(), Some("/some/path/ending"));
    /// assert_eq!(bn.next(), Some("/some/path/ending/in"));
    /// assert_eq!(bn.next(), Some("/some/path/ending/in/foo"));
    /// assert_eq!(bn.next(), None);
    /// ```
    pub fn dirnames<T: AsRef<str> + ?Sized>(s: &T) -> impl Iterator<Item = &str> {
        let s = s.as_ref();
        if s == "/" {
            DirNames::Root(true)
        } else {
            DirNames::Path { cur: s, all: s, base: 1 }
        }
    }

    /// Return the number of levels in the path.
    ///
    /// # Examples
    /// ```
    /// use netidx::path::Path;
    /// let p = Path::from("/foo/bar/baz");
    /// assert_eq!(Path::levels(&p), 3);
    /// ```
    pub fn levels<T: AsRef<str> + ?Sized>(s: &T) -> usize {
        let mut p = 0;
        for _ in Path::parts(s) {
            p += 1
        }
        p
    }

    /// return the path without the last part, or return None if the
    /// path is empty or /.
    ///
    /// # Examples
    /// ```
    /// use netidx::path::Path;
    /// let p = Path::from("/foo/bar/baz");
    /// assert_eq!(Path::dirname(&p), Some("/foo/bar"));
    ///
    /// let p = Path::root();
    /// assert_eq!(Path::dirname(&p), None);
    ///
    /// let p = Path::from("/foo");
    /// assert_eq!(Path::dirname(&p), None);
    /// ```
    pub fn dirname<T: AsRef<str> + ?Sized>(s: &T) -> Option<&str> {
        let s = s.as_ref();
        Path::rfind_sep(s).and_then(|i| if i == 0 { None } else { Some(&s[0..i]) })
    }

    pub fn dirname_with_sep<T: AsRef<str> + ?Sized>(s: &T) -> Option<&str> {
        let s = s.as_ref();
        Path::rfind_sep(s).and_then(|i| if i == 0 { None } else { Some(&s[0..i+1]) })
    }

    /// return the last part of the path, or return None if the path
    /// is empty.
    ///
    /// # Examples
    /// ```
    /// use netidx::path::Path;
    /// let p = Path::from("/foo/bar/baz");
    /// assert_eq!(Path::basename(&p), Some("baz"));
    ///
    /// let p = Path::from("foo");
    /// assert_eq!(Path::basename(&p), Some("foo"));
    ///
    /// let p = Path::from("foo/bar");
    /// assert_eq!(Path::basename(&p), Some("bar"));
    ///
    /// let p = Path::from("");
    /// assert_eq!(Path::basename(&p), None);
    ///
    /// let p = Path::from("/");
    /// assert_eq!(Path::basename(&p), None);
    /// ```
    pub fn basename<T: AsRef<str> + ?Sized>(s: &T) -> Option<&str> {
        let s = s.as_ref();
        match Path::rfind_sep(s) {
            None => {
                if s.len() > 0 {
                    Some(s)
                } else {
                    None
                }
            }
            Some(i) => {
                if s.len() <= 1 {
                    None
                } else {
                    Some(&s[i + 1..s.len()])
                }
            }
        }
    }

    fn find_sep_int<F: Fn(&str) -> Option<usize>>(mut s: &str, f: F) -> Option<usize> {
        if s.len() == 0 {
            None
        } else {
            loop {
                match f(s) {
                    None => return None,
                    Some(i) => {
                        if !utils::is_escaped(s, SEP, ESC, i) {
                            return Some(i);
                        } else {
                            s = &s[0..i];
                        }
                    }
                }
            }
        }
    }

    /// return the position of the last path separator in the path, or
    /// None if there isn't one.
    ///
    /// # Examples
    /// ```
    /// use netidx::path::Path;
    /// let p = Path::from("/foo/bar/baz");
    /// assert_eq!(Path::rfind_sep(&p), Some(8));
    ///
    /// let p = Path::from("");
    /// assert_eq!(Path::rfind_sep(&p), None);
    /// ```
    pub fn rfind_sep<T: AsRef<str> + ?Sized>(s: &T) -> Option<usize> {
        let s = s.as_ref();
        Path::find_sep_int(s, |s| s.rfind(SEP))
    }

    /// return the position of the first path separator in the path, or
    /// None if there isn't one.
    ///
    /// # Examples
    /// ```
    /// use netidx::path::Path;
    /// let p = Path::from("foo/bar/baz");
    /// assert_eq!(Path::find_sep(&p), Some(3));
    ///
    /// let p = Path::from("");
    /// assert_eq!(Path::find_sep(&p), None);
    /// ```
    pub fn find_sep<T: AsRef<str> + ?Sized>(s: &T) -> Option<usize> {
        let s = s.as_ref();
        Path::find_sep_int(s, |s| s.find(SEP))
    }
}
