use anyhow::{bail, Result};
use turbo_tasks::{Upcast, Value, ValueToString, Vc};

use super::ChunkableModule;
use crate::{
    asset::Asset,
    context::AssetContext,
    module::Module,
    reference_type::{EntryReferenceSubType, ReferenceType},
    source::Source,
};

/// Marker trait for the chunking context to accept evaluated entries.
///
/// The chunking context implementation will resolve the dynamic entry to a
/// well-known value or trait object.
#[turbo_tasks::value_trait]
pub trait EvaluatableAsset: Asset + Module + ChunkableModule {}

pub trait EvaluatableAssetExt {
    fn to_evaluatable(
        self: Vc<Self>,
        context: Vc<Box<dyn AssetContext>>,
    ) -> Vc<Box<dyn EvaluatableAsset>>;
}

impl<T> EvaluatableAssetExt for T
where
    T: Upcast<Box<dyn Source>>,
{
    fn to_evaluatable(
        self: Vc<Self>,
        context: Vc<Box<dyn AssetContext>>,
    ) -> Vc<Box<dyn EvaluatableAsset>> {
        to_evaluatable(Vc::upcast(self), context)
    }
}

#[turbo_tasks::function]
async fn to_evaluatable(
    asset: Vc<Box<dyn Source>>,
    context: Vc<Box<dyn AssetContext>>,
) -> Result<Vc<Box<dyn EvaluatableAsset>>> {
    let asset = context.process(
        asset,
        Value::new(ReferenceType::Entry(EntryReferenceSubType::Runtime)),
    );
    let Some(entry) = Vc::try_resolve_downcast::<Box<dyn EvaluatableAsset>>(asset).await? else {
        bail!(
            "{} is not a valid evaluated entry",
            asset.ident().to_string().await?
        )
    };
    Ok(entry)
}

#[turbo_tasks::value(transparent)]
pub struct EvaluatableAssets(Vec<Vc<Box<dyn EvaluatableAsset>>>);

#[turbo_tasks::value_impl]
impl EvaluatableAssets {
    #[turbo_tasks::function]
    pub fn empty() -> Vc<EvaluatableAssets> {
        EvaluatableAssets(vec![]).cell()
    }

    #[turbo_tasks::function]
    pub fn one(entry: Vc<Box<dyn EvaluatableAsset>>) -> Vc<EvaluatableAssets> {
        EvaluatableAssets(vec![entry]).cell()
    }

    #[turbo_tasks::function]
    pub fn many(assets: Vec<Vc<Box<dyn EvaluatableAsset>>>) -> Vc<EvaluatableAssets> {
        EvaluatableAssets(assets).cell()
    }

    #[turbo_tasks::function]
    pub async fn with_entry(
        self: Vc<Self>,
        entry: Vc<Box<dyn EvaluatableAsset>>,
    ) -> Result<Vc<EvaluatableAssets>> {
        let mut entries = self.await?.clone_value();
        entries.push(entry);
        Ok(EvaluatableAssets(entries).cell())
    }
}
