//! Fixed-capacity Linux-style path normalization for the serial shell.

use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PathError {
    Invalid,
    TooLong,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct ShellPath {
    bytes: [u8; littlefs::MAX_NAME_LEN],
    len: usize,
}

impl ShellPath {
    pub(super) const fn root() -> Self {
        Self {
            bytes: [0; littlefs::MAX_NAME_LEN],
            len: 0,
        }
    }

    pub(super) fn resolve(cwd: &Self, input: &str) -> Result<Self, PathError> {
        if input.as_bytes().contains(&0) {
            return Err(PathError::Invalid);
        }

        let mut output = if input.starts_with('/') {
            Self::root()
        } else {
            *cwd
        };
        for component in input.split('/') {
            match component {
                "" | "." => {}
                ".." => output.pop(),
                component => output.push(component)?,
            }
        }
        Ok(output)
    }

    pub(super) fn as_key(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    pub(super) const fn is_root(&self) -> bool {
        self.len == 0
    }

    pub(super) fn is_ancestor_of(&self, other: &Self) -> bool {
        self.is_root()
            || self == other
            || (other.as_key().starts_with(self.as_key())
                && other.as_key().as_bytes().get(self.len) == Some(&b'/'))
    }

    pub(super) fn rebase(&mut self, old: &Self, new: &Self) -> Result<(), PathError> {
        if !old.is_ancestor_of(self) {
            return Ok(());
        }
        let suffix = &self.as_key().as_bytes()[old.len..];
        let new_len = new
            .len
            .checked_add(suffix.len())
            .ok_or(PathError::TooLong)?;
        if new_len > self.bytes.len() {
            return Err(PathError::TooLong);
        }

        let mut rebased = Self::root();
        rebased.bytes[..new.len].copy_from_slice(&new.bytes[..new.len]);
        rebased.bytes[new.len..new_len].copy_from_slice(suffix);
        rebased.len = new_len;
        *self = rebased;
        Ok(())
    }

    fn push(&mut self, component: &str) -> Result<(), PathError> {
        if component.is_empty() || component == "." || component == ".." {
            return Err(PathError::Invalid);
        }
        let separator = usize::from(self.len != 0);
        let new_len = self
            .len
            .checked_add(separator)
            .and_then(|len| len.checked_add(component.len()))
            .ok_or(PathError::TooLong)?;
        if new_len > self.bytes.len() {
            return Err(PathError::TooLong);
        }
        if separator != 0 {
            self.bytes[self.len] = b'/';
            self.len += 1;
        }
        self.bytes[self.len..new_len].copy_from_slice(component.as_bytes());
        self.len = new_len;
        Ok(())
    }

    fn pop(&mut self) {
        self.len = self.as_key().rfind('/').unwrap_or(0);
    }
}

impl fmt::Display for ShellPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            formatter.write_str("/")
        } else {
            write!(formatter, "/{}", self.as_key())
        }
    }
}
