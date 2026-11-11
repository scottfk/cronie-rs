//! A read-only stdio `FILE` over a crontab, with glibc's semantics for what
//! the parser uses: `getc`, `ungetc`, `ftell`, `fseek` and `feof`, plus
//! cronie's `get_char()`/`unget_char()`/`get_string()`/`skip_comments()`
//! (misc.c), which keep the global line number.
//!
//! glibc details that matter here:
//! - end-of-file is sticky (since glibc 2.28): once `getc` returns EOF it
//!   keeps returning EOF until `ungetc`, `fseek` or `clearerr`;
//! - `ungetc(EOF)` does nothing;
//! - pushed-back characters are returned last-in first-out, even when they
//!   differ from what was read (cronie's `get_number()` can push the same
//!   character back twice);
//! - `ftell` accounts for pushed-back characters.

use crate::globals;

pub const EOF: i32 = -1;

#[derive(Debug, Clone)]
pub struct Reader {
    data: Vec<u8>,
    pos: usize,
    back: Vec<u8>,
    eof: bool,
    /// Read lazily from this fd, as stdio does (`None`: `data` is all).
    src: Option<crate::sys::Fd>,
}

impl Reader {
    pub fn new(data: Vec<u8>) -> Reader {
        Reader { data, pos: 0, back: Vec::new(), eof: false, src: None }
    }

    /// A stream over an open fd (`fdopen(fd, "r")`), read as needed.
    pub fn from_fd(fd: crate::sys::Fd) -> Reader {
        Reader { data: Vec::new(), pos: 0, back: Vec::new(), eof: false, src: Some(fd) }
    }

    pub fn fd(&self) -> Option<crate::sys::Fd> {
        self.src
    }

    /// Refill like `_IO_file_underflow`: one read(2) of up to 4 kB.
    fn fill(&mut self) -> bool {
        let Some(fd) = self.src else { return false };
        let mut buf = [0u8; 4096];
        loop {
            let n = crate::sys::read(fd, &mut buf);
            if n < 0 && crate::sys::cerrno() == libc::EINTR {
                continue;
            }
            if n <= 0 {
                return false;
            }
            self.data.extend_from_slice(&buf[..n as usize]);
            return true;
        }
    }

    /// `rewind()`: back to the start of the file (on a pipe, which can't
    /// seek, only the EOF indicator is reset).
    pub fn rewind(&mut self) {
        self.back.clear();
        self.eof = false;
        match self.src {
            Some(fd) => {
                if crate::sys::lseek_set(fd, 0).is_ok() {
                    self.data.clear();
                    self.pos = 0;
                }
            }
            None => self.pos = 0,
        }
    }

    /// `fgets(buf, size, f)`: up to size-1 bytes or through a newline;
    /// `None` at EOF with nothing read.
    pub fn fgets(&mut self, size: usize) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        while out.len() + 1 < size {
            let c = self.getc();
            if c == EOF {
                break;
            }
            out.push(c as u8);
            if c == i32::from(b'\n') {
                break;
            }
        }
        if out.is_empty() && size > 1 { None } else { Some(out) }
    }

    /// `getc()`.
    pub fn getc(&mut self) -> i32 {
        if let Some(c) = self.back.pop() {
            return i32::from(c);
        }
        if self.eof {
            return EOF;
        }
        if self.pos >= self.data.len() {
            self.fill();
        }
        match self.data.get(self.pos) {
            Some(&c) => {
                self.pos += 1;
                i32::from(c)
            }
            None => {
                self.eof = true;
                EOF
            }
        }
    }

    /// `ungetc()`.
    pub fn ungetc(&mut self, ch: i32) {
        if ch == EOF {
            return;
        }
        self.eof = false;
        let c = ch as u8;
        if self.back.is_empty() && self.pos > 0 && self.data[self.pos - 1] == c {
            self.pos -= 1;
        } else {
            self.back.push(c);
        }
    }

    /// `ftell()`.
    pub fn tell(&self) -> usize {
        self.pos.saturating_sub(self.back.len())
    }

    /// `fseek(f, pos, SEEK_SET)`.
    pub fn seek(&mut self, pos: usize) {
        self.pos = pos;
        self.back.clear();
        self.eof = false;
    }

    /// `feof()`.
    pub fn feof(&self) -> bool {
        self.eof
    }
}

/// `get_char()`: `getc`, counting lines.
pub fn get_char(f: &mut Reader) -> i32 {
    let ch = f.getc();
    if ch == i32::from(b'\n') {
        globals::set_line_number(globals::line_number() + 1);
    }
    ch
}

/// `unget_char()`: `ungetc`, uncounting lines.
pub fn unget_char(ch: i32, f: &mut Reader) {
    f.ungetc(ch);
    if ch == i32::from(b'\n') {
        globals::set_line_number(globals::line_number() - 1);
    }
}

/// `get_string(string, size, file, terms)`: read up to a terminator (or
/// EOF), keeping at most `size - 1` bytes.  Returns the terminator.
pub fn get_string(out: &mut Vec<u8>, size: usize, f: &mut Reader, terms: &[u8]) -> i32 {
    out.clear();
    let mut room = size;
    loop {
        let ch = get_char(f);
        if ch == EOF || terms.contains(&(ch as u8)) {
            return ch;
        }
        if room > 1 {
            out.push(ch as u8);
            room -= 1;
        }
    }
}

/// `Skip_Blanks(c, f)`.
pub fn skip_blanks(mut ch: i32, f: &mut Reader) -> i32 {
    while ch == i32::from(b'\t') || ch == i32::from(b' ') {
        ch = get_char(f);
    }
    ch
}

/// `Skip_Nonblanks(c, f)`.
pub fn skip_nonblanks(mut ch: i32, f: &mut Reader) -> i32 {
    while ch != i32::from(b'\t') && ch != i32::from(b' ') && ch != i32::from(b'\n') && ch != EOF {
        ch = get_char(f);
    }
    ch
}

/// `MAX_GARBAGE`: comments and blank lines allowed between entries.
pub const MAX_GARBAGE: usize = 32768;

/// `skip_comments()`: skip blank lines and comment lines.  False if there
/// were more than MAX_GARBAGE characters of them.
pub fn skip_comments(f: &mut Reader) -> bool {
    let mut n = 0usize;
    let mut ch;
    loop {
        ch = get_char(f);
        if ch == EOF {
            break;
        }
        n += 1;
        if n > MAX_GARBAGE {
            return false;
        }
        while ch == i32::from(b' ') || ch == i32::from(b'\t') {
            ch = get_char(f);
            n += 1;
            if n > MAX_GARBAGE {
                return false;
            }
        }
        if ch == EOF {
            break;
        }
        if ch != i32::from(b'\n') && ch != i32::from(b'#') {
            break;
        }
        while ch != i32::from(b'\n') && ch != EOF {
            ch = get_char(f);
            n += 1;
            if n > MAX_GARBAGE {
                return false;
            }
        }
    }
    if ch != EOF {
        unget_char(ch, f);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushback_and_sticky_eof() {
        let mut r = Reader::new(b"ab".to_vec());
        assert_eq!(r.getc(), i32::from(b'a'));
        assert_eq!(r.getc(), i32::from(b'b'));
        assert_eq!(r.getc(), EOF);
        assert!(r.feof());
        assert_eq!(r.getc(), EOF);
        r.ungetc(i32::from(b'b'));
        assert!(!r.feof());
        // A second push of the same char doesn't match 'a': it's stacked.
        r.ungetc(i32::from(b'b'));
        assert_eq!(r.tell(), 0);
        assert_eq!(r.getc(), i32::from(b'b'));
        assert_eq!(r.getc(), i32::from(b'b'));
        assert_eq!(r.getc(), EOF);
        r.seek(0);
        assert_eq!(r.getc(), i32::from(b'a'));
    }
}
