//! Hexadecimal text of captured bytes.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HexError {
    #[error("{0:?} is not a hexadecimal digit")]
    NotADigit(char),
    #[error("the text has an odd number of hexadecimal digits")]
    OddLength,
}

/// Read bytes from hexadecimal text.
///
/// Whitespace and commas between digits are ignored, so the output of `xxd -p` and the hex text
/// of a descriptor capture (bytes of two digits, separated by blanks, in lines of 16) both work.
pub fn parse(text: &str) -> Result<Vec<u8>, HexError> {
    let digits = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ',')
        .map(|c| {
            c.to_digit(16)
                .map(|digit| digit as u8)
                .ok_or(HexError::NotADigit(c))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if digits.len() % 2 != 0 {
        return Err(HexError::OddLength);
    }
    Ok(digits
        .chunks(2)
        .map(|pair| pair[0] << 4 | pair[1])
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_with_blanks_and_lines_are_read() {
        assert_eq!(
            parse(" 12 01 00 02\n 09 00 ff,A0\r\n").unwrap(),
            [0x12, 0x01, 0x00, 0x02, 0x09, 0x00, 0xff, 0xa0]
        );
        assert_eq!(parse("1201").unwrap(), [0x12, 0x01]);
        assert_eq!(parse("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn bad_text_is_refused() {
        assert_eq!(parse("123"), Err(HexError::OddLength));
        assert_eq!(parse("1g"), Err(HexError::NotADigit('g')));
        assert_eq!(parse("0x12"), Err(HexError::NotADigit('x')));
    }
}
