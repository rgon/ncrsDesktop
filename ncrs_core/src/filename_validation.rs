use std::ffi::OsStr;

#[derive(Debug)]
pub enum FilenameError {
    Empty,
    TooLong,
    InvalidCharacter(char),
    ControlCharacter,
    TrailingSpaceOrDot,
    ReservedName,
}

impl FilenameError {
    pub fn to_errno(&self) -> i32 {
        match self {
            FilenameError::TooLong => libc::ENAMETOOLONG,
            _ => libc::EINVAL,
        }
    }
}

impl std::fmt::Display for FilenameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilenameError::Empty => write!(f, "filename cannot be empty"),
            FilenameError::TooLong => write!(f, "filename exceeds 250 characters"),
            FilenameError::InvalidCharacter(c) => write!(f, "invalid character '{}' in filename", c),
            FilenameError::ControlCharacter => write!(f, "control characters not allowed in filename"),
            FilenameError::TrailingSpaceOrDot => write!(f, "filename cannot end with space or dot"),
            FilenameError::ReservedName => write!(f, "reserved filename"),
        }
    }
}

const INVALID_CHARS: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL",
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn validate(name: &OsStr) -> Result<(), FilenameError> {
    let name = name.to_string_lossy();

    if name.is_empty() {
        return Err(FilenameError::Empty);
    }
    if name.len() > 250 {
        return Err(FilenameError::TooLong);
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return Err(FilenameError::TrailingSpaceOrDot);
    }
    for c in name.chars() {
        if c < '\u{0020}' {
            return Err(FilenameError::ControlCharacter);
        }
        if INVALID_CHARS.contains(&c) {
            return Err(FilenameError::InvalidCharacter(c));
        }
    }
    let stem = name.split('.').next().unwrap_or(&name);
    if RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        return Err(FilenameError::ReservedName);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn valid_names() {
        assert!(validate(OsStr::new("hello.txt")).is_ok());
        assert!(validate(OsStr::new("my file (1).pdf")).is_ok());
        assert!(validate(OsStr::new(".hidden")).is_ok());
        assert!(validate(OsStr::new("a")).is_ok());
    }

    #[test]
    fn empty() {
        assert!(matches!(validate(OsStr::new("")), Err(FilenameError::Empty)));
    }

    #[test]
    fn too_long() {
        let long = "a".repeat(251);
        assert!(matches!(validate(OsStr::new(&long)), Err(FilenameError::TooLong)));
    }

    #[test]
    fn invalid_chars() {
        for c in INVALID_CHARS {
            let name = format!("file{}name", c);
            assert!(matches!(validate(OsStr::new(&name)), Err(FilenameError::InvalidCharacter(_))));
        }
    }

    #[test]
    fn control_chars() {
        assert!(matches!(validate(OsStr::new("file\x01")), Err(FilenameError::ControlCharacter)));
    }

    #[test]
    fn trailing_space_or_dot() {
        assert!(matches!(validate(OsStr::new("file ")), Err(FilenameError::TrailingSpaceOrDot)));
        assert!(matches!(validate(OsStr::new("file.")), Err(FilenameError::TrailingSpaceOrDot)));
    }

    #[test]
    fn reserved_names() {
        assert!(matches!(validate(OsStr::new("CON")), Err(FilenameError::ReservedName)));
        assert!(matches!(validate(OsStr::new("con")), Err(FilenameError::ReservedName)));
        assert!(matches!(validate(OsStr::new("COM1.txt")), Err(FilenameError::ReservedName)));
        assert!(matches!(validate(OsStr::new("nul")), Err(FilenameError::ReservedName)));
    }

    #[test]
    fn not_reserved_when_not_stem() {
        assert!(validate(OsStr::new("CONX")).is_ok());
        assert!(validate(OsStr::new("console")).is_ok());
    }
}
