use std::sync::Arc;

use anyhow::Result;
use turbopack_core::source_map::SourceMap;

#[turbo_tasks::value(serialization = "auto_for_input")]
#[derive(PartialOrd, Ord, Hash, Debug, Copy, Clone)]
pub enum CssInputTransform {
    Nested,
    Custom,
}

#[turbo_tasks::value(transparent, serialization = "auto_for_input")]
#[derive(Debug, PartialOrd, Ord, Hash, Clone)]
pub struct CssInputTransforms(Vec<CssInputTransform>);

pub struct TransformContext<'a> {
    pub source_map: &'a Arc<SourceMap>,
}

impl CssInputTransform {
    pub async fn apply(
        &self,
        stylesheet: &mut Stylesheet,
        &TransformContext { source_map: _ }: &TransformContext<'_>,
    ) -> Result<()> {
        match *self {
            CssInputTransform::Nested => {
                stylesheet.visit_mut_with(&mut swc_css_compat::compiler::Compiler::new(
                    swc_css_compat::compiler::Config {
                        process: swc_css_compat::feature::Features::NESTING,
                    },
                ));
            }
            CssInputTransform::Custom => todo!(),
        }
        Ok(())
    }
}
