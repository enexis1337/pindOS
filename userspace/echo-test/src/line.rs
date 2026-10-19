/// Максимум байтов строки без префикса и перевода строки.
pub const LINE_CAPACITY: usize = 256;
const PREFIX: &[u8] = b"[echo] ";

pub struct Line {
    buf: [u8; PREFIX.len() + LINE_CAPACITY + 1],
    len: usize,
    skip_lf: bool,
}

impl Line {
    pub fn new() -> Self {
        let mut line = Self {
            buf: [0; PREFIX.len() + LINE_CAPACITY + 1],
            len: PREFIX.len(),
            skip_lf: false,
        };
        line.buf[..PREFIX.len()].copy_from_slice(PREFIX);
        line
    }

    /// Возвращает готовую строку для одного write. Лишние байты отбрасываются
    /// до конца строки; CRLF считается одним разделителем, в том числе между read.
    pub fn push(&mut self, byte: u8) -> Option<&[u8]> {
        if byte == b'\n' && self.skip_lf {
            self.skip_lf = false;
            return None;
        }
        self.skip_lf = byte == b'\r';
        if byte == b'\r' || byte == b'\n' {
            let end = self.len;
            self.buf[end] = b'\n';
            self.len = PREFIX.len();
            return Some(&self.buf[..end + 1]);
        }
        if self.len < PREFIX.len() + LINE_CAPACITY {
            self.buf[self.len] = byte;
            self.len += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(input: &[u8]) -> Vec<Vec<u8>> {
        let mut line = Line::new();
        input.iter().filter_map(|&b| line.push(b).map(|s| s.to_vec())).collect()
    }

    #[test]
    fn cr_lf_and_repeated_lines() {
        assert_eq!(collect(b"one\rtwo\nthree\r\nfour\r"),
            vec![b"[echo] one\n".to_vec(), b"[echo] two\n".to_vec(),
                 b"[echo] three\n".to_vec(), b"[echo] four\n".to_vec()]);
    }

    #[test]
    fn incomplete_line_is_retained_across_blocks() {
        let mut line = Line::new();
        for &byte in b"hel" { assert!(line.push(byte).is_none()); }
        for &byte in b"lo" { assert!(line.push(byte).is_none()); }
        assert_eq!(line.push(b'\r'), Some(&b"[echo] hello\n"[..]));
        assert!(line.push(b'\n').is_none());
        assert_eq!(line.push(b'\n'), Some(&b"[echo] \n"[..]));
    }

    #[test]
    fn truncation_discards_until_delimiter_and_next_line_works() {
        let mut input = vec![b'x'; LINE_CAPACITY * 4];
        input.extend_from_slice(b"\rnext\n");
        let output = collect(&input);
        let mut expected = PREFIX.to_vec();
        expected.extend_from_slice(&[b'x'; LINE_CAPACITY]);
        expected.push(b'\n');
        assert_eq!(output, vec![expected, b"[echo] next\n".to_vec()]);
    }

    #[test]
    fn exactly_full_and_empty_lines() {
        let mut input = vec![b'a'; LINE_CAPACITY];
        input.extend_from_slice(b"\n\r\r");
        let output = collect(&input);
        assert_eq!(output.len(), 3);
        assert_eq!(output[0].len(), PREFIX.len() + LINE_CAPACITY + 1);
        assert_eq!(output[1], b"[echo] \n");
        assert_eq!(output[2], b"[echo] \n");
    }
}
