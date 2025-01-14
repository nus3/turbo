use anyhow::Result;
use turbo_tasks::{ValueToString, Vc};
use turbo_tasks_fs::{FileSystemEntryType, FileSystemPath};

use super::AssetReference;
use crate::{file_source::FileSource, resolve::ResolveResult};

#[turbo_tasks::value]
pub struct SourceMapReference {
    from: Vc<FileSystemPath>,
    file: Vc<FileSystemPath>,
}

#[turbo_tasks::value_impl]
impl SourceMapReference {
    #[turbo_tasks::function]
    pub fn new(from: Vc<FileSystemPath>, file: Vc<FileSystemPath>) -> Vc<Self> {
        Self::cell(SourceMapReference { from, file })
    }
}

#[turbo_tasks::value_impl]
impl AssetReference for SourceMapReference {
    #[turbo_tasks::function]
    async fn resolve_reference(&self) -> Vc<ResolveResult> {
        let file_type = self.file.get_type().await;
        if let Ok(file_type_result) = file_type.as_ref() {
            if let FileSystemEntryType::File = &**file_type_result {
                return ResolveResult::asset(Vc::upcast(FileSource::new(self.file))).into();
            }
        }
        ResolveResult::unresolveable().into()
    }
}

#[turbo_tasks::value_impl]
impl ValueToString for SourceMapReference {
    #[turbo_tasks::function]
    async fn to_string(&self) -> Result<Vc<String>> {
        Ok(Vc::cell(format!(
            "source map file is referenced by {}",
            self.from.to_string().await?
        )))
    }
}
