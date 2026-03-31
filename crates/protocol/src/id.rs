// SPDX-License-Identifier: BSD-3-Clause

use std::fmt::Display;
use std::str::FromStr;

use once_cell::sync::Lazy;
use regex::Regex;

use narwhal_util::string_atom::StringAtom;

/// The maximum length of a username.
const USERNAME_MAX_LENGTH: usize = 256;

/// The maximum length of a channel handler.
const CHANNEL_HANDLER_MAX_LENGTH: usize = 256;

const LOCALHOST_DOMAIN: &str = "localhost";

/// The regex used to validate the domain.
///
/// This matches:
/// - Standard domain names (e.g., example.com, sub.example.com)
/// - IPv4 addresses (e.g., 192.168.1.1)
/// - IPv6 addresses in square brackets (e.g., [2001:db8::1])
/// - Optional port numbers (e.g., example.com:8080)
static DOMAIN_REGEX: Lazy<Regex> = Lazy::new(|| {
  Regex::new(
    r"^(?:(?:[a-zA-Z0-9-]{1,63}\.)+[a-zA-Z]{2,63}|(?:\[[0-9a-fA-F:]+\])|(?:\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}))(?::\d{1,5})?$"
  ).unwrap()
});

const DOMAIN_MAX_LENGTH: usize = 253;

/// Validates a domain.
fn validate_domain(domain: &str) -> bool {
  if domain.is_empty() || domain.len() > DOMAIN_MAX_LENGTH {
    return false;
  }
  domain == LOCALHOST_DOMAIN || DOMAIN_REGEX.is_match(domain)
}

/// The error type for channel ID parsing.
#[derive(Clone, Debug, PartialEq)]
pub enum ChannelIdParsingError {
  /// The channel ID has an invalid format.
  InvalidChannelIdFormat,
}

// === impl ChannelIdParsingError ===

impl Display for ChannelIdParsingError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::InvalidChannelIdFormat => write!(f, "invalid channel ID format"),
    }
  }
}

impl std::error::Error for ChannelIdParsingError {}

/// The error type for NID parsing.
#[derive(Clone, Debug, PartialEq)]
pub enum NidParsingError {
  /// The NID has an invalid format.
  InvalidNidFormat,
}

// === impl NidParsingError ===

impl Display for NidParsingError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::InvalidNidFormat => write!(f, "invalid NID format"),
    }
  }
}

impl std::error::Error for NidParsingError {}

/// The channel ID used by the client/server to identify a channel.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChannelId {
  /// The channel handler.
  pub handler: StringAtom,

  /// The domain of the channel.
  pub domain: StringAtom,

  /// The full representation of the channel ID.
  full: StringAtom,
}

// === impl ChannelId ===

impl ChannelId {
  /// Creates a new channel ID.
  pub fn new(handler: StringAtom, domain: StringAtom) -> Result<Self, ChannelIdParsingError> {
    let handler_ref = handler.as_ref();
    let domain_ref = domain.as_ref();

    if !Self::validate(handler_ref, domain_ref) {
      return Err(ChannelIdParsingError::InvalidChannelIdFormat);
    }
    Ok(Self { handler: handler.clone(), domain: domain.clone(), full: Self::full_atom(handler, domain) })
  }

  /// Creates a new channel ID without validation.
  ///
  /// # Safety
  ///
  /// This method skips validation checks on the channel and domain.
  /// The caller must ensure that the provided values are valid.
  pub fn new_unchecked(handler: StringAtom, domain: StringAtom) -> Self {
    Self { handler: handler.clone(), domain: domain.clone(), full: Self::full_atom(handler, domain) }
  }

  /// Validates a channel ID.
  fn validate(handler: &str, domain: &str) -> bool {
    if handler.is_empty() || handler.len() > CHANNEL_HANDLER_MAX_LENGTH {
      return false;
    }
    if !handler.chars().all(|c| c.is_alphanumeric()) {
      return false;
    }

    validate_domain(domain)
  }

  fn full_atom(handler: StringAtom, domain: StringAtom) -> StringAtom {
    format!("!{}@{}", handler, domain).into()
  }
}

impl From<&ChannelId> for StringAtom {
  fn from(ch: &ChannelId) -> Self {
    ch.full.clone()
  }
}

impl From<ChannelId> for StringAtom {
  fn from(ch: ChannelId) -> Self {
    ch.full.clone()
  }
}

impl TryFrom<StringAtom> for ChannelId {
  type Error = ChannelIdParsingError;

  fn try_from(a: StringAtom) -> Result<Self, Self::Error> {
    ChannelId::from_str(a.as_ref())
  }
}

impl Display for ChannelId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "!{}@{}", self.handler, self.domain)
  }
}

impl FromStr for ChannelId {
  type Err = ChannelIdParsingError;

  fn from_str(full: &str) -> Result<Self, Self::Err> {
    // Check if string starts with '!'
    if !full.starts_with('!') {
      return Err(ChannelIdParsingError::InvalidChannelIdFormat);
    }

    // Find the '@' position
    let at_pos = match full.find('@') {
      Some(pos) => pos,
      None => return Err(ChannelIdParsingError::InvalidChannelIdFormat),
    };

    // Extract the channel and domain parts
    let channel_str = &full[1..at_pos];
    let domain_str = &full[at_pos + 1..];

    if !Self::validate(channel_str, domain_str) {
      return Err(ChannelIdParsingError::InvalidChannelIdFormat);
    }

    Ok(Self { handler: channel_str.into(), domain: domain_str.into(), full: StringAtom::from(full) })
  }
}

/// The NID used by the client/server to be authenticated.
///
/// A NID consists of a username and domain in the format `username@domain`.
/// Server NIDs have an empty username and are represented simply as `domain`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Nid {
  /// The username of the NID.
  pub username: StringAtom,

  /// The domain of the NID.
  pub domain: StringAtom,

  /// The full representation of the NID.
  full: StringAtom,
}

// === impl Nid ===

impl Nid {
  /// Creates a new NID validating the username and domain.
  pub fn new(username: StringAtom, domain: StringAtom) -> Result<Self, NidParsingError> {
    let username_ref = username.as_ref();
    let domain_ref = domain.as_ref();

    if !Self::validate(username_ref, domain_ref) {
      return Err(NidParsingError::InvalidNidFormat);
    }
    Ok(Self { username: username.clone(), domain: domain.clone(), full: Self::full_atom(username, domain) })
  }

  /// Creates a new NID without validation.
  ///
  /// # Safety
  ///
  /// This method skips validation checks on the username and domain.
  /// The caller must ensure that the provided values are valid.
  pub fn new_unchecked(username: StringAtom, domain: StringAtom) -> Self {
    Self { username: username.clone(), domain: domain.clone(), full: Self::full_atom(username, domain) }
  }

  /// Tells if the NID is a server NID.
  ///
  /// Server NIDs have an empty username and are represented simply as `domain`.
  pub fn is_server(&self) -> bool {
    self.username.is_empty()
  }

  /// Validates a username and domain.
  fn validate(username: &str, domain: &str) -> bool {
    // Username validation
    if username.len() > USERNAME_MAX_LENGTH {
      return false;
    }
    if !username.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '.' || c == '_') {
      return false;
    }

    validate_domain(domain)
  }

  fn full_atom(username: StringAtom, domain: StringAtom) -> StringAtom {
    if username.is_empty() { domain.clone() } else { StringAtom::from(format!("{}@{}", username, domain)) }
  }
}

impl From<&Nid> for StringAtom {
  fn from(z: &Nid) -> Self {
    z.full.clone()
  }
}

impl From<Nid> for StringAtom {
  fn from(z: Nid) -> Self {
    z.full.clone()
  }
}

impl TryFrom<StringAtom> for Nid {
  type Error = NidParsingError;

  fn try_from(a: StringAtom) -> Result<Self, Self::Error> {
    Nid::from_str(a.as_ref())
  }
}

impl FromStr for Nid {
  type Err = NidParsingError;

  fn from_str(full: &str) -> Result<Self, Self::Err> {
    let (username, domain) = {
      if let Some((username, domain)) = full.split_once('@') {
        if username.is_empty() {
          return Err(NidParsingError::InvalidNidFormat);
        }
        (username, domain)
      } else {
        ("", full)
      }
    };

    if !Self::validate(username, domain) {
      return Err(NidParsingError::InvalidNidFormat);
    }

    Ok(Self { username: StringAtom::from(username), domain: StringAtom::from(domain), full: StringAtom::from(full) })
  }
}

impl Display for Nid {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    if self.is_server() { write!(f, "{}", self.domain) } else { write!(f, "{}@{}", self.username, self.domain) }
  }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Nid {
  fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(self.full.as_ref())
  }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Nid {
  fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    let s: &str = serde::Deserialize::deserialize(deserializer)?;
    Nid::from_str(s).map_err(serde::de::Error::custom)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_valid_channel_id_from_str() {
    // Basic valid case with numeric handler
    let channel_id = ChannelId::from_str("!123@example.com").unwrap();
    assert_eq!(channel_id.handler.as_ref(), "123");
    assert_eq!(channel_id.domain.as_ref(), "example.com");
    assert_eq!(channel_id.to_string(), "!123@example.com");

    // Test with larger handler number
    let channel_id = ChannelId::from_str("!987654321@domain.test").unwrap();
    assert_eq!(channel_id.handler.as_ref(), "987654321");
    assert_eq!(channel_id.domain.as_ref(), "domain.test");
    assert_eq!(channel_id.to_string(), "!987654321@domain.test");

    // Test with subdomain
    let channel_id = ChannelId::from_str("!42@sub.example.org").unwrap();
    assert_eq!(channel_id.handler.as_ref(), "42");
    assert_eq!(channel_id.domain.as_ref(), "sub.example.org");
    assert_eq!(channel_id.to_string(), "!42@sub.example.org");

    // Test with alphanumeric handler
    let channel_id = ChannelId::from_str("!abc123@example.com").unwrap();
    assert_eq!(channel_id.handler.as_ref(), "abc123");
    assert_eq!(channel_id.domain.as_ref(), "example.com");
    assert_eq!(channel_id.to_string(), "!abc123@example.com");

    // Test with localhost domain
    let channel_id = ChannelId::from_str("!test@localhost").unwrap();
    assert_eq!(channel_id.handler.as_ref(), "test");
    assert_eq!(channel_id.domain.as_ref(), "localhost");
    assert_eq!(channel_id.to_string(), "!test@localhost");
  }

  #[test]
  fn test_invalid_channel_id_from_str() {
    // Missing ! prefix
    assert!(ChannelId::from_str("abc123@example.com").is_err());

    // Missing @ symbol
    assert!(ChannelId::from_str("!abc123example.com").is_err());

    // Empty channel
    assert!(ChannelId::from_str("!@example.com").is_err());

    // Invalid channel (non-alphanumeric)
    assert!(ChannelId::from_str("!*abc123*@localhost").is_err());

    // Empty domain
    assert!(ChannelId::from_str("!abc123@").is_err());

    // Invalid domain format
    assert!(ChannelId::from_str("!abc123@invalid..domain").is_err());
  }

  #[test]
  fn test_nid_try_from() {
    use std::collections::HashMap;

    let test_cases: HashMap<&str, Result<(&str, &str), NidParsingError>> = HashMap::from([
      // valid cases
      ("user@example.com", Ok(("user", "example.com"))),
      ("user@sub.example.com", Ok(("user", "sub.example.com"))),
      ("example.com", Ok(("", "example.com"))),
      // invalid cases
      ("@example.com", Err(NidParsingError::InvalidNidFormat)),
      ("user@", Err(NidParsingError::InvalidNidFormat)),
      ("user@host@example.com", Err(NidParsingError::InvalidNidFormat)),
      ("", Err(NidParsingError::InvalidNidFormat)),
      (" user@example.com", Err(NidParsingError::InvalidNidFormat)),
      ("user@example.com ", Err(NidParsingError::InvalidNidFormat)),
    ]);

    for (input, expected) in test_cases {
      let result = Nid::from_str(input);
      match expected {
        Ok((expected_username, expected_domain)) => {
          assert!(result.is_ok(), "expected Ok for input '{}', got {:?}", input, result);
          let nid = result.unwrap();
          assert_eq!(nid.username.as_ref(), expected_username, "unexpected username for input '{}'", input);
          assert_eq!(nid.domain.as_ref(), expected_domain, "unexpected domain for input '{}'", input);
        },
        Err(expected_err) => {
          assert!(result.is_err(), "expected Err for input '{}', got {:?}", input, result);
          assert_eq!(result.unwrap_err(), expected_err, "unexpected error for input '{}'", input);
        },
      }
    }
  }

  #[test]
  fn test_nid_try_from_atom() {
    use std::collections::HashMap;

    let test_cases: HashMap<StringAtom, Result<(&str, &str), NidParsingError>> = HashMap::from([
      // valid cases
      (StringAtom::from("user@example.com"), Ok(("user", "example.com"))),
      (StringAtom::from("user@sub.example.com"), Ok(("user", "sub.example.com"))),
      (StringAtom::from("example.com"), Ok(("", "example.com"))),
      // invalid cases
      (StringAtom::from("@example.com"), Err(NidParsingError::InvalidNidFormat)),
      (StringAtom::from("user@"), Err(NidParsingError::InvalidNidFormat)),
      (StringAtom::from("user@host@example.com"), Err(NidParsingError::InvalidNidFormat)),
      (StringAtom::from(""), Err(NidParsingError::InvalidNidFormat)),
      (StringAtom::from(" user@example.com"), Err(NidParsingError::InvalidNidFormat)),
      (StringAtom::from("user@example.com "), Err(NidParsingError::InvalidNidFormat)),
    ]);

    for (input, expected) in test_cases {
      let result = Nid::try_from(input.clone());
      match expected {
        Ok((expected_username, expected_domain)) => {
          assert!(result.is_ok(), "expected Ok for input '{}', got {:?}", input, result);
          let nid = result.unwrap();
          assert_eq!(nid.username.as_ref(), expected_username, "unexpected username for input '{}'", input);
          assert_eq!(nid.domain.as_ref(), expected_domain, "unexpected domain for input '{}'", input);
        },
        Err(expected_err) => {
          assert!(result.is_err(), "expected Err for input '{}', got {:?}", input, result);
          assert_eq!(result.unwrap_err(), expected_err, "unexpected error for input '{}'", input);
        },
      }
    }
  }
}
