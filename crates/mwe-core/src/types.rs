// SPDX-License-Identifier: AGPL-3.0-or-later
//! Canonical shared types and identifiers.
//!
//! IDs: `wiki_id`, `page_id`, `region_id`, `fact_id` (`UUIDv7`), `sender_id`,
//! `device_label`, `consumer_id`, `wiki_type_id`. ACL data model (`Principal`,
//! `Acl`), region marker attributes (`RegionAttrs`).
//!
//! ## `fact_id` format
//!
//! R.1.3 (decisions.md, round 2026-05-17) picks **`UUIDv7` lowercase with
//! dashes** for `fact_index.fact_id`. The marker grammar in `schemi.md §3.1`
//! still shows the older `f-YYYY-MM-DD-NNN` form in EBNF and in all examples
//! at §3.3; that text is stale relative to R.1.3 and will be refreshed in a
//! later planning round. The code in this crate follows R.1.3.
//!
//! ## `catalog_id` format
//!
//! `catalog_id` keeps the `c-YYYY-MM-DD-<kind>-NNN.<ext>` shape of
//! `schemi.md §3.1`, no decision supersedes it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

// ---------- serde (string wire form, shared by Principal + FactId) ----------

impl Serialize for Principal {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Principal {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse::<Self>().map_err(serde::de::Error::custom)
    }
}

impl Serialize for FactId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FactId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

// ---------- Principal ----------

/// A principal an ACL marker can name.
///
/// Every principal is a **user** or a **group** — there is no separate
/// "global" variant. Public/world reach is the builtin **`global` group**,
/// to which every user implicitly belongs: a region naming `global` (in
/// `owner`, `allow`, or `sender`) is therefore readable by anyone. The
/// global group also carries an operator-editable `scope` like any other
/// group; see [`crate::enrollment`] and the engineering wiki
/// (`identity-and-acl.md`).
///
/// Wire format:
/// - the global group ↔ `"global"` (bare, no prefix — back-compatible with
///   legacy `owner=global` markers; `"group:global"` parses identically)
/// - `User(id)` ↔ `"user:<id>"`
/// - `Group(id)` ↔ `"group:<id>"`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Principal {
    /// The named user.
    User(String),
    /// Members of the named group. The builtin `global` group (id
    /// `"global"`) has universal membership — see [`Principal::is_global`].
    Group(String),
}

impl Principal {
    /// The builtin universal group: everyone is a member, so a region
    /// naming it is public. Canonical id `"global"`.
    #[must_use]
    pub fn global() -> Self {
        Self::Group("global".to_owned())
    }

    /// Whether this principal is the builtin universal `global` group.
    #[must_use]
    pub fn is_global(&self) -> bool {
        matches!(self, Self::Group(id) if id == "global")
    }
}

/// Reason a string did not parse as a [`Principal`].
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PrincipalParseError {
    /// Empty input.
    #[error("empty principal")]
    Empty,
    /// Missing `user:`/`group:` prefix (and not the literal `global`).
    #[error("missing user:/group: prefix in {0:?}")]
    MissingPrefix(String),
    /// Prefix present but identifier empty.
    #[error("empty identifier after prefix in {0:?}")]
    EmptyId(String),
}

impl FromStr for Principal {
    type Err = PrincipalParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(PrincipalParseError::Empty);
        }
        if s == "global" {
            return Ok(Self::global());
        }
        if let Some(id) = s.strip_prefix("user:") {
            return if id.is_empty() {
                Err(PrincipalParseError::EmptyId(s.to_owned()))
            } else {
                Ok(Self::User(id.to_owned()))
            };
        }
        if let Some(id) = s.strip_prefix("group:") {
            return if id.is_empty() {
                Err(PrincipalParseError::EmptyId(s.to_owned()))
            } else {
                Ok(Self::Group(id.to_owned()))
            };
        }
        Err(PrincipalParseError::MissingPrefix(s.to_owned()))
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(id) => write!(f, "user:{id}"),
            // The builtin global group keeps its bare wire form for
            // back-compat with legacy `owner=global` markers.
            Self::Group(id) if id == "global" => f.write_str("global"),
            Self::Group(id) => write!(f, "group:{id}"),
        }
    }
}

// ---------- Acl ----------

/// Region ACL: `owner` (the fact's *subject*) + optional allow-list.
///
/// **`owner` names the fact's SUBJECT — who or what the region is
/// *about*.** It is *not* who wrote it (that is `sender`, the provenance)
/// and *not* who may read it (that is `allow`, the audience): the three
/// are independent axes. The "owner" name is deliberate, not a leftover
/// from a classic file-ownership model — in mwe-mcp the data subject
/// *governs* the fact about them (ACL changes are owner-or-admin), so the
/// subject genuinely owns the datum on themselves. Read `owner` as
/// *subject* everywhere; never as "creator" or "visibility". The
/// engineering wiki (`concepts/identity-and-acl.md`) is the SSOT for the
/// model.
///
/// `owner == None` means "inherit `acl_default` from the enclosing
/// `_meta.md`". The parser builds [`Acl`] from the marker's `owner=` and
/// `allow=` attributes; if `owner=` is absent the parser leaves it as
/// `None`, and `render` later resolves it against `acl_default`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Acl {
    /// The fact's **subject** — the single principal the region is *about*
    /// (not its author `sender`, not its audience `allow`). `None` ⇒
    /// inherit `acl_default`.
    pub owner: Option<Principal>,
    /// Additional principals extended by `allow=…` — the *audience* axis,
    /// who may read it beyond owner + sender (possibly empty).
    pub allow: Vec<Principal>,
}

// ---------- FactId ----------

/// A `fact_id` in canonical [`UUIDv7`] lowercase-with-dash form (R.1.3).
///
/// Example: `018f1234-5678-7abc-9def-0123456789ab`.
///
/// Construction goes through [`FactId::parse`], which enforces:
/// - exact length 36
/// - dashes at byte positions 8, 13, 18, 23
/// - all other characters in `[0-9a-f]`
/// - version nibble (position 14) equal to `'7'` (`UUIDv7`)
/// - variant nibble (position 19) in `{8, 9, a, b}` (RFC 4122 variant 1)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FactId(String);

/// Reason a string did not parse as a [`FactId`].
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum FactIdParseError {
    /// Length, dash positions, charset, version or variant were wrong.
    #[error("invalid fact_id {0:?}: expected UUIDv7 (lowercase, hyphenated)")]
    InvalidFormat(String),
}

impl FactId {
    /// Parse a `UUIDv7`-formatted `fact_id` (see type-level docs for the
    /// rules).
    ///
    /// # Errors
    ///
    /// Returns [`FactIdParseError::InvalidFormat`] when `s` is not a
    /// well-formed `UUIDv7` in canonical lowercase form.
    pub fn parse(s: &str) -> Result<Self, FactIdParseError> {
        const DASH_POSITIONS: [usize; 4] = [8, 13, 18, 23];

        let bytes = s.as_bytes();
        if bytes.len() != 36 {
            return Err(FactIdParseError::InvalidFormat(s.to_owned()));
        }
        for (i, &b) in bytes.iter().enumerate() {
            if DASH_POSITIONS.contains(&i) {
                if b != b'-' {
                    return Err(FactIdParseError::InvalidFormat(s.to_owned()));
                }
            } else if !is_ascii_lower_hex(b) {
                return Err(FactIdParseError::InvalidFormat(s.to_owned()));
            }
        }
        // Version: first nibble of the 3rd group, byte position 14.
        if bytes[14] != b'7' {
            return Err(FactIdParseError::InvalidFormat(s.to_owned()));
        }
        // Variant: first nibble of the 4th group, byte position 19, ∈ {8,9,a,b}.
        if !matches!(bytes[19], b'8' | b'9' | b'a' | b'b') {
            return Err(FactIdParseError::InvalidFormat(s.to_owned()));
        }
        Ok(Self(s.to_owned()))
    }

    /// Borrow the canonical string form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FactId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

const fn is_ascii_lower_hex(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'f')
}

// ---------- CatalogId ----------

/// A `catalog_id` in the form `c-YYYY-MM-DD-<kind>-NNN.<ext>` (see the
/// marker grammar).
///
/// Example: `c-2026-05-10-photo-001.jpg`. `<kind>` is lowercase ASCII
/// letters, `<ext>` is lowercase ASCII alnum, `NNN` is ≥1 ASCII digit.
/// The parser deliberately accepts **any** `[a-z]+` kind — legacy ids and
/// imported archives stay valid input forever; the canonical English
/// vocabulary (`photo` / `video` / `audio` / `doc`) is enforced at minting
/// time by [`crate::media`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CatalogId(String);

/// Reason a string did not parse as a [`CatalogId`].
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum CatalogIdParseError {
    /// The string did not match `c-YYYY-MM-DD-<kind>-NNN.<ext>`.
    #[error("invalid catalog_id {0:?}: expected c-YYYY-MM-DD-<kind>-NNN.<ext>")]
    InvalidFormat(String),
}

impl CatalogId {
    /// Parse a canonical `catalog_id` (see type-level docs).
    ///
    /// # Errors
    ///
    /// Returns [`CatalogIdParseError::InvalidFormat`] on any deviation from
    /// the canonical shape.
    pub fn parse(s: &str) -> Result<Self, CatalogIdParseError> {
        let Some(rest) = s.strip_prefix("c-") else {
            return Err(CatalogIdParseError::InvalidFormat(s.to_owned()));
        };
        // Split into YYYY, MM, DD, kind, "NNN.ext". `splitn(5, '-')` keeps
        // any embedded `-` inside the last part — there shouldn't be any
        // but if there were the last group would absorb them and the
        // validation below would catch them via the digit check on NNN.
        let parts: Vec<&str> = rest.splitn(5, '-').collect();
        if parts.len() != 5 {
            return Err(CatalogIdParseError::InvalidFormat(s.to_owned()));
        }
        let valid_date = parts[0].len() == 4
            && parts[1].len() == 2
            && parts[2].len() == 2
            && parts[0].bytes().all(|b| b.is_ascii_digit())
            && parts[1].bytes().all(|b| b.is_ascii_digit())
            && parts[2].bytes().all(|b| b.is_ascii_digit());
        let valid_kind = !parts[3].is_empty() && parts[3].bytes().all(|b| b.is_ascii_lowercase());
        let last = parts[4];
        let Some(dot) = last.rfind('.') else {
            return Err(CatalogIdParseError::InvalidFormat(s.to_owned()));
        };
        let (nnn, ext) = (&last[..dot], &last[dot + 1..]);
        let valid_nnn = !nnn.is_empty() && nnn.bytes().all(|b| b.is_ascii_digit());
        let valid_ext = !ext.is_empty()
            && ext
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        if valid_date && valid_kind && valid_nnn && valid_ext {
            Ok(Self(s.to_owned()))
        } else {
            Err(CatalogIdParseError::InvalidFormat(s.to_owned()))
        }
    }

    /// Borrow the canonical string form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CatalogId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------- WikiSlug ----------

/// A directory-name slug for a memory wiki node.
///
/// Charset: `[a-z0-9-]` plus the literal `°` used by the slug-collision
/// resolver (Samvise convention, see [`crate::slug::resolve_collision`]).
/// Must be non-empty, must not start or end with `-`, must not contain two
/// consecutive `-`.
///
/// Construction normally goes through [`crate::slug::derive_slug`] (and the
/// collision resolver), then [`WikiSlug::parse`] for validation; the resolver
/// emits a string the parser accepts, so the round-trip is closed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WikiSlug(String);

/// Reason a string did not parse as a [`WikiSlug`].
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum WikiSlugParseError {
    /// Empty input.
    #[error("empty wiki slug")]
    Empty,
    /// First or last character is `-` (would produce a `.` or trailing dash
    /// directory name once placed on disk).
    #[error("wiki slug {0:?}: leading or trailing dash")]
    EdgeDash(String),
    /// Two `-` in a row (the slug pipeline collapses these, so seeing them
    /// here means the caller hand-built a non-canonical slug).
    #[error("wiki slug {0:?}: consecutive dashes")]
    DoubleDash(String),
    /// Character outside `[a-z0-9°-]`.
    #[error("wiki slug {0:?}: invalid character {1:?}")]
    InvalidChar(String, char),
}

impl WikiSlug {
    /// Parse and validate a slug string.
    ///
    /// # Errors
    ///
    /// Returns a [`WikiSlugParseError`] when the input violates the charset
    /// or edge-dash rules documented at the type level.
    pub fn parse(s: &str) -> Result<Self, WikiSlugParseError> {
        if s.is_empty() {
            return Err(WikiSlugParseError::Empty);
        }
        if s.starts_with('-') || s.ends_with('-') {
            return Err(WikiSlugParseError::EdgeDash(s.to_owned()));
        }
        if s.contains("--") {
            return Err(WikiSlugParseError::DoubleDash(s.to_owned()));
        }
        for c in s.chars() {
            let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '°';
            if !ok {
                return Err(WikiSlugParseError::InvalidChar(s.to_owned(), c));
            }
        }
        Ok(Self(s.to_owned()))
    }

    /// Borrow the canonical string form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WikiSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------- WikiId ----------

/// Stable identifier for a memory-wiki node.
///
/// Derived once from the directory chain at forge time
/// (`alice-acmecorp-widget-pro` for `wiki/alice/acmecorp/widget-pro/`) and
/// then stored in `_meta.md.wiki_id`. Survives a rename of the path on disk
/// because the renamed `_meta.md` keeps the old id.
///
/// Charset: same as [`WikiSlug`] (the id is a join of slugs with `-`), with
/// the same edge-dash and double-dash restrictions. The root wiki uses the
/// special id `"root"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WikiId(String);

/// Reason a string did not parse as a [`WikiId`].
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum WikiIdParseError {
    /// Empty input.
    #[error("empty wiki id")]
    Empty,
    /// First or last character is `-`.
    #[error("wiki id {0:?}: leading or trailing dash")]
    EdgeDash(String),
    /// Two `-` in a row.
    #[error("wiki id {0:?}: consecutive dashes")]
    DoubleDash(String),
    /// Character outside `[a-z0-9°-]`.
    #[error("wiki id {0:?}: invalid character {1:?}")]
    InvalidChar(String, char),
}

impl WikiId {
    /// The well-known id of the wiki-root (the workdir's `wiki/` root).
    pub const ROOT: &'static str = "root";

    /// Parse and validate a wiki-id string.
    ///
    /// # Errors
    ///
    /// Returns a [`WikiIdParseError`] when the input violates the charset
    /// or edge-dash rules documented at the type level.
    pub fn parse(s: &str) -> Result<Self, WikiIdParseError> {
        if s.is_empty() {
            return Err(WikiIdParseError::Empty);
        }
        if s.starts_with('-') || s.ends_with('-') {
            return Err(WikiIdParseError::EdgeDash(s.to_owned()));
        }
        if s.contains("--") {
            return Err(WikiIdParseError::DoubleDash(s.to_owned()));
        }
        for c in s.chars() {
            let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '°';
            if !ok {
                return Err(WikiIdParseError::InvalidChar(s.to_owned(), c));
            }
        }
        Ok(Self(s.to_owned()))
    }

    /// Construct the `root` wiki id (the wiki-root).
    #[must_use]
    pub fn root() -> Self {
        Self(Self::ROOT.to_owned())
    }

    /// Join a parent id with a child slug to produce the canonical id of
    /// the child wiki: `parent-childslug`. If `parent` is the root, the
    /// returned id is just the slug (so the root's children become
    /// `alice`, `bob`, … rather than `root-alice`).
    #[must_use]
    pub fn child_of(parent: &Self, child: &WikiSlug) -> Self {
        if parent.0 == Self::ROOT {
            Self(child.0.clone())
        } else {
            Self(format!("{}-{}", parent.0, child.0))
        }
    }

    /// Borrow the canonical string form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True if this id refers to the root wiki.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0 == Self::ROOT
    }
}

impl fmt::Display for WikiId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------- RegionAttrs ----------

/// Attributes carried by a region opening marker `{{…}}`.
///
/// Built by the parser from the four supported attributes:
/// - `owner=<principal>` → `acl.owner` (the fact's *subject* — who it is
///   *about*, not its author or audience; see [`Acl`])
/// - `allow=<principal>(,<principal>)*` → `acl.allow`
/// - `sender=<principal>` → `sender` (cross-user attribution, see
///   `modello-memoria.md §4`)
/// - `f=<UUIDv7>` → `fact_id`
///
/// All four are optional. `owner == None` means the region inherits
/// `acl_default` from the enclosing `_meta.md`. `sender == None` means
/// auto-attribution (sender = owner, common case).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionAttrs {
    /// Region-level access control list.
    pub acl: Acl,
    /// Cross-user attribution. `None` ⇒ sender equals owner (the common
    /// case).
    pub sender: Option<Principal>,
    /// `fact_id` from `f=…`. `None` for regions used only as ACL wrappers
    /// (no fact promotion).
    pub fact_id: Option<FactId>,
}

/// Reference `UUIDv7` used across crate tests (parser, acl, render).
///
/// Hand-crafted: arbitrary timestamp prefix `018f1234-5678`, version
/// nibble `7`, variant nibble `9`.
#[cfg(test)]
pub(crate) const SAMPLE_UUID_V7: &str = "018f1234-5678-7abc-9def-0123456789ab";

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- Principal ----------

    #[test]
    fn principal_roundtrip_global() {
        let p: Principal = "global".parse().unwrap();
        assert_eq!(p, Principal::global());
        assert_eq!(p.to_string(), "global");
    }

    #[test]
    fn principal_roundtrip_user() {
        let p: Principal = "user:alice".parse().unwrap();
        assert_eq!(p, Principal::User("alice".into()));
        assert_eq!(p.to_string(), "user:alice");
    }

    #[test]
    fn principal_roundtrip_group() {
        let p: Principal = "group:team".parse().unwrap();
        assert_eq!(p, Principal::Group("team".into()));
        assert_eq!(p.to_string(), "group:team");
    }

    #[test]
    fn principal_rejects_bare_name() {
        let err = "alice".parse::<Principal>().unwrap_err();
        assert!(matches!(err, PrincipalParseError::MissingPrefix(s) if s == "alice"));
    }

    #[test]
    fn principal_rejects_empty() {
        let err = "".parse::<Principal>().unwrap_err();
        assert!(matches!(err, PrincipalParseError::Empty));
    }

    #[test]
    fn principal_rejects_empty_id_after_prefix() {
        assert!(matches!(
            "user:".parse::<Principal>().unwrap_err(),
            PrincipalParseError::EmptyId(_)
        ));
        assert!(matches!(
            "group:".parse::<Principal>().unwrap_err(),
            PrincipalParseError::EmptyId(_)
        ));
    }

    // ---------- FactId ----------

    #[test]
    fn fact_id_parses_canonical_uuidv7() {
        let f = FactId::parse(SAMPLE_UUID_V7).unwrap();
        assert_eq!(f.as_str(), SAMPLE_UUID_V7);
        assert_eq!(f.to_string(), SAMPLE_UUID_V7);
    }

    #[test]
    fn fact_id_rejects_wrong_length() {
        assert!(FactId::parse("").is_err());
        assert!(FactId::parse("not-a-uuid").is_err());
        assert!(FactId::parse(&format!("{SAMPLE_UUID_V7}-extra")).is_err());
    }

    #[test]
    fn fact_id_rejects_wrong_version() {
        // Replace version digit at byte 14 with '4' (UUIDv4 instead of v7).
        let mut bytes = SAMPLE_UUID_V7.as_bytes().to_vec();
        bytes[14] = b'4';
        let s = String::from_utf8(bytes).unwrap();
        assert!(FactId::parse(&s).is_err());
    }

    #[test]
    fn fact_id_rejects_wrong_variant() {
        // Replace variant digit at byte 19 with 'c' (must be 8/9/a/b).
        let mut bytes = SAMPLE_UUID_V7.as_bytes().to_vec();
        bytes[19] = b'c';
        let s = String::from_utf8(bytes).unwrap();
        assert!(FactId::parse(&s).is_err());
    }

    #[test]
    fn fact_id_rejects_uppercase_hex() {
        let upper = SAMPLE_UUID_V7.to_uppercase();
        assert!(FactId::parse(&upper).is_err());
    }

    #[test]
    fn fact_id_rejects_legacy_date_nnn_format() {
        // Documents the supersede: schemi.md §3.1 EBNF form no longer
        // validates after R.1.3.
        assert!(FactId::parse("f-2026-05-11-001").is_err());
    }

    // ---------- CatalogId ----------

    #[test]
    fn catalog_id_canonical_examples() {
        assert!(CatalogId::parse("c-2026-05-10-foto-001.jpg").is_ok());
        assert!(CatalogId::parse("c-2026-05-10-doc-01.pdf").is_ok());
        assert!(CatalogId::parse("c-2026-05-11-vid-1.mp4").is_ok());
        assert!(CatalogId::parse("c-2026-05-10-audio-007.m4a").is_ok());
    }

    #[test]
    fn catalog_id_rejects_missing_prefix() {
        assert!(CatalogId::parse("2026-05-10-foto-001.jpg").is_err());
    }

    #[test]
    fn catalog_id_rejects_missing_extension() {
        assert!(CatalogId::parse("c-2026-05-10-foto-001").is_err());
    }

    #[test]
    fn catalog_id_rejects_uppercase() {
        assert!(CatalogId::parse("c-2026-05-10-FOTO-001.jpg").is_err());
        assert!(CatalogId::parse("c-2026-05-10-foto-001.JPG").is_err());
    }

    #[test]
    fn catalog_id_rejects_bad_date() {
        assert!(CatalogId::parse("c-26-05-10-foto-001.jpg").is_err()); // 2-digit year
        assert!(CatalogId::parse("c-2026-5-10-foto-001.jpg").is_err()); // 1-digit month
    }

    #[test]
    fn catalog_id_rejects_nondigit_seq() {
        assert!(CatalogId::parse("c-2026-05-10-foto-abc.jpg").is_err());
    }

    #[test]
    fn catalog_id_roundtrip() {
        let s = "c-2026-05-10-foto-001.jpg";
        let id = CatalogId::parse(s).unwrap();
        assert_eq!(id.as_str(), s);
        assert_eq!(id.to_string(), s);
    }

    // ---------- WikiSlug ----------

    #[test]
    fn wiki_slug_accepts_canonical_forms() {
        assert!(WikiSlug::parse("alice").is_ok());
        assert!(WikiSlug::parse("widget-pro").is_ok());
        assert!(WikiSlug::parse("adr-024").is_ok());
        // Samvise collision marker.
        assert!(WikiSlug::parse("bilbo°2").is_ok());
        assert!(WikiSlug::parse("bilbo°99").is_ok());
    }

    #[test]
    fn wiki_slug_rejects_empty() {
        assert!(matches!(
            WikiSlug::parse(""),
            Err(WikiSlugParseError::Empty)
        ));
    }

    #[test]
    fn wiki_slug_rejects_edge_dashes() {
        assert!(matches!(
            WikiSlug::parse("-alice"),
            Err(WikiSlugParseError::EdgeDash(_))
        ));
        assert!(matches!(
            WikiSlug::parse("alice-"),
            Err(WikiSlugParseError::EdgeDash(_))
        ));
    }

    #[test]
    fn wiki_slug_rejects_double_dash() {
        assert!(matches!(
            WikiSlug::parse("widget--pro"),
            Err(WikiSlugParseError::DoubleDash(_))
        ));
    }

    #[test]
    fn wiki_slug_rejects_uppercase_and_punct_and_dot() {
        assert!(matches!(
            WikiSlug::parse("Alice"),
            Err(WikiSlugParseError::InvalidChar(_, 'A'))
        ));
        assert!(matches!(
            WikiSlug::parse("alice_bob"),
            Err(WikiSlugParseError::InvalidChar(_, '_'))
        ));
        // Crucially: `.` is forbidden so a slug can never be confused with a
        // path traversal segment.
        assert!(matches!(
            WikiSlug::parse(".."),
            Err(WikiSlugParseError::InvalidChar(_, '.'))
        ));
    }

    // ---------- WikiId ----------

    #[test]
    fn wiki_id_root_constants() {
        let r = WikiId::root();
        assert!(r.is_root());
        assert_eq!(r.as_str(), "root");
        assert_eq!(WikiId::ROOT, "root");
    }

    #[test]
    fn wiki_id_child_of_root_uses_bare_slug() {
        let root = WikiId::root();
        let alice = WikiSlug::parse("alice").unwrap();
        let id = WikiId::child_of(&root, &alice);
        assert_eq!(id.as_str(), "alice");
        assert!(!id.is_root());
    }

    #[test]
    fn wiki_id_child_of_nonroot_joins_with_dash() {
        let alice = WikiId::parse("alice").unwrap();
        let acme = WikiSlug::parse("acmecorp").unwrap();
        let id = WikiId::child_of(&alice, &acme);
        assert_eq!(id.as_str(), "alice-acmecorp");
        let widget = WikiSlug::parse("widget-pro").unwrap();
        let id2 = WikiId::child_of(&id, &widget);
        assert_eq!(id2.as_str(), "alice-acmecorp-widget-pro");
    }

    #[test]
    fn wiki_id_parse_rejects_invalid_forms() {
        assert!(matches!(WikiId::parse(""), Err(WikiIdParseError::Empty)));
        assert!(matches!(
            WikiId::parse("-alice"),
            Err(WikiIdParseError::EdgeDash(_))
        ));
        assert!(matches!(
            WikiId::parse("alice--bob"),
            Err(WikiIdParseError::DoubleDash(_))
        ));
        assert!(matches!(
            WikiId::parse("Alice"),
            Err(WikiIdParseError::InvalidChar(_, 'A'))
        ));
    }
}
