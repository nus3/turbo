use std::{io::Write, ops::Deref, sync::Arc};

use anyhow::Result;
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sourcemap::{SourceMap as CrateMap, SourceMapBuilder};
use turbo_tasks::{TryJoinIterExt, Vc};
use turbo_tasks_fs::rope::{Rope, RopeBuilder};

use crate::source_pos::SourcePos;

pub(crate) mod source_map_asset;

pub use source_map_asset::SourceMapAsset;

/// Allows callers to generate source maps.
#[turbo_tasks::value_trait]
pub trait GenerateSourceMap {
    /// Generates a usable source map, capable of both tracing and stringifying.
    fn generate_source_map(self: Vc<Self>) -> Vc<OptionSourceMap>;

    /// Returns an individual section of the larger source map, if found.
    fn by_section(self: Vc<Self>, _section: String) -> Vc<OptionSourceMap> {
        Vc::cell(None)
    }
}

/// The source map spec lists 2 formats, a regular format where a single map
/// covers the entire file, and an "index" sectioned format where multiple maps
/// cover different regions of the file.
#[turbo_tasks::value(shared)]
pub enum SourceMap {
    /// A regular source map covers an entire file.
    Regular(#[turbo_tasks(trace_ignore)] RegularSourceMap),
    /// A sectioned source map contains many (possibly recursive) maps covering
    /// different regions of the file.
    Sectioned(#[turbo_tasks(trace_ignore)] SectionedSourceMap),
}

#[turbo_tasks::value(transparent)]
pub struct SectionMapping(IndexMap<String, Vc<Box<dyn GenerateSourceMap>>>);

#[turbo_tasks::value(transparent)]
pub struct OptionSourceMap(Option<Vc<SourceMap>>);

/// A token represents a mapping in a source map. It may either be Synthetic,
/// meaning it was generated by some build tool and doesn't represent a location
/// in a user-authored source file, or it is Original, meaning it represents a
/// real location in source file.
#[turbo_tasks::value]
pub enum Token {
    Synthetic(SyntheticToken),
    Original(OriginalToken),
}

/// A SyntheticToken represents a region of the generated file that was created
/// by some build tool.
#[turbo_tasks::value]
pub struct SyntheticToken {
    generated_line: usize,
    generated_column: usize,
}

/// An OriginalToken represents a region of the generated file that exists in
/// user-authored source file.
#[turbo_tasks::value]
pub struct OriginalToken {
    pub generated_line: usize,
    pub generated_column: usize,
    pub original_file: String,
    pub original_line: usize,
    pub original_column: usize,
    pub name: Option<String>,
}

#[turbo_tasks::value(transparent)]
pub struct OptionToken(Option<Token>);

impl<'a> From<sourcemap::Token<'a>> for Token {
    fn from(t: sourcemap::Token) -> Self {
        if t.has_source() {
            Token::Original(OriginalToken {
                generated_line: t.get_dst_line() as usize,
                generated_column: t.get_dst_col() as usize,
                original_file: t
                    .get_source()
                    .expect("already checked token has source")
                    .to_string(),
                original_line: t.get_src_line() as usize,
                original_column: t.get_src_col() as usize,
                name: t.get_name().map(String::from),
            })
        } else {
            Token::Synthetic(SyntheticToken {
                generated_line: t.get_dst_line() as usize,
                generated_column: t.get_dst_col() as usize,
            })
        }
    }
}

impl SourceMap {
    /// Creates a new SourceMap::Regular Vc out of a sourcemap::SourceMap
    /// ("CrateMap") instance.
    pub fn new_regular(map: CrateMap) -> Self {
        SourceMap::Regular(RegularSourceMap::new(map))
    }

    /// Creates a new SourceMap::Sectioned Vc out of a collection of source map
    /// sections.
    pub fn new_sectioned(sections: Vec<SourceMapSection>) -> Self {
        SourceMap::Sectioned(SectionedSourceMap::new(sections))
    }
}

#[turbo_tasks::value_impl]
impl SourceMap {
    /// A source map that contains no actual source location information (no
    /// `sources`, no mappings that point into a source). This is used to tell
    /// Chrome that the generated code starting at a particular offset is no
    /// longer part of the previous section's mappings.
    #[turbo_tasks::function]
    pub fn empty() -> Vc<Self> {
        let mut builder = SourceMapBuilder::new(None);
        builder.add(0, 0, 0, 0, None, None);
        SourceMap::new_regular(builder.into_sourcemap()).cell()
    }
}

#[turbo_tasks::value_impl]
impl SourceMap {
    /// Stringifies the source map into JSON bytes.
    #[turbo_tasks::function]
    pub async fn to_rope(self: Vc<Self>) -> Result<Vc<Rope>> {
        let this = self.await?;
        let rope = match &*this {
            SourceMap::Regular(r) => {
                let mut bytes = vec![];
                r.0.to_writer(&mut bytes)?;
                Rope::from(bytes)
            }

            SourceMap::Sectioned(s) => {
                if s.sections.len() == 1 {
                    let s = &s.sections[0];
                    if s.offset == (0, 0) {
                        return Ok(s.map.to_rope());
                    }
                }

                // My kingdom for a decent dedent macro with interpolation!
                let mut rope = RopeBuilder::from(
                    r#"{
  "version": 3,
  "sections": ["#,
                );

                let sections = s
                    .sections
                    .iter()
                    .map(|s| async move { Ok((s.offset, s.map.to_rope().await?)) })
                    .try_join()
                    .await?;

                let mut first_section = true;
                for (offset, section_map) in sections {
                    if !first_section {
                        rope += ",";
                    }
                    first_section = false;

                    write!(
                        rope,
                        r#"
    {{"offset": {{"line": {}, "column": {}}}, "map": "#,
                        offset.line, offset.column,
                    )?;

                    rope += &*section_map;

                    rope += "}";
                }

                rope += "]
}";

                rope.build()
            }
        };
        Ok(rope.cell())
    }

    /// Traces a generated line/column into an mapping token representing either
    /// synthetic code or user-authored original code.
    #[turbo_tasks::function]
    pub async fn lookup_token(
        self: Vc<Self>,
        line: usize,
        column: usize,
    ) -> Result<Vc<OptionToken>> {
        let token = match &*self.await? {
            SourceMap::Regular(map) => map
                .lookup_token(line as u32, column as u32)
                // The sourcemap crate incorrectly returns a previous line's token when there's
                // not a match on this line.
                .filter(|t| t.get_dst_line() == line as u32)
                .map(Token::from),

            SourceMap::Sectioned(map) => {
                let len = map.sections.len();
                let mut low = 0;
                let mut high = len;
                let pos = SourcePos { line, column };

                // A "greatest lower bound" binary search. We're looking for the closest section
                // offset <= to our line/col.
                while low < high {
                    let mid = (low + high) / 2;
                    if pos < map.sections[mid].offset {
                        high = mid;
                    } else {
                        low = mid + 1;
                    }
                }

                // Our GLB search will return the section immediately to the right of the
                // section we actually want to recurse into, because the binary search does not
                // early exit on an exact match (it'll `low = mid + 1`).
                if low > 0 && low <= len {
                    let SourceMapSection { map, offset } = &map.sections[low - 1];
                    // We're looking for the position `l` lines into region covered by this
                    // sourcemap's section.
                    let l = line - offset.line;
                    // The source map starts offset by the section's column only on its first line.
                    // On the 2nd+ line, the source map covers starting at column 0.
                    let c = if line == offset.line {
                        column - offset.column
                    } else {
                        column
                    };
                    return Ok(map.lookup_token(l, c));
                }
                None
            }
        };
        Ok(OptionToken(token).cell())
    }
}

/// A regular source map covers an entire file.
#[derive(Debug, Serialize, Deserialize)]
pub struct RegularSourceMap(Arc<CrateMapWrapper>);

impl RegularSourceMap {
    fn new(map: CrateMap) -> Self {
        RegularSourceMap(Arc::new(CrateMapWrapper(map)))
    }
}

impl Deref for RegularSourceMap {
    type Target = Arc<CrateMapWrapper>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Eq for RegularSourceMap {}
impl PartialEq for RegularSourceMap {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Wraps the CrateMap struct so that it can be cached in a Vc.
#[derive(Debug)]
pub struct CrateMapWrapper(sourcemap::SourceMap);

// Safety: CrateMap contains a raw pointer, which isn't Send, which is required
// to cache in a Vc. So, we have wrap it in 4 layers of cruft to do it. We don't
// actually use the pointer, because we don't perform sourcesContent lookups,
// so it's fine.
unsafe impl Send for CrateMapWrapper {}
unsafe impl Sync for CrateMapWrapper {}

impl Deref for CrateMapWrapper {
    type Target = sourcemap::SourceMap;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Serialize for CrateMapWrapper {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error;
        let mut bytes = vec![];
        self.0.to_writer(&mut bytes).map_err(Error::custom)?;
        serializer.serialize_bytes(bytes.as_slice())
    }
}

impl<'de> Deserialize<'de> for CrateMapWrapper {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let bytes = <&[u8]>::deserialize(deserializer)?;
        let map = CrateMap::from_slice(bytes).map_err(Error::custom)?;
        Ok(CrateMapWrapper(map))
    }
}

/// A sectioned source map contains many (possibly recursive) maps covering
/// different regions of the file.
#[derive(Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct SectionedSourceMap {
    sections: Vec<SourceMapSection>,
}

impl SectionedSourceMap {
    pub fn new(sections: Vec<SourceMapSection>) -> Self {
        Self { sections }
    }
}

/// A section of a larger sectioned source map, which applies at source
/// positions >= the offset (until the next section starts).
#[derive(Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct SourceMapSection {
    offset: SourcePos,
    map: Vc<SourceMap>,
}

impl SourceMapSection {
    pub fn new(offset: SourcePos, map: Vc<SourceMap>) -> Self {
        Self { offset, map }
    }
}
