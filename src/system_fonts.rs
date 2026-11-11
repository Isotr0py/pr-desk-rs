use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily, FontTweak, epaint::text::VariationCoords};
use fontique::{
    Collection, CollectionOptions, GenericFamily, QueryFamily, QueryStatus, SourceCache,
    SourceCacheOptions,
};

const SOURCE_HAN_NAMES: &[&str] = &[
    "Source Han Sans SC",
    "Source Han Sans CN",
    "Noto Sans CJK SC",
    "Noto Sans SC",
    "思源黑体",
];
const JETBRAINS_MONO_NAMES: &[&str] = &["JetBrains Mono"];

#[derive(Clone)]
struct LoadedFont {
    name: String,
    data: FontData,
}

pub fn install(ctx: &egui::Context) {
    let mut collection = Collection::new(CollectionOptions {
        shared: false,
        system_fonts: true,
    });
    let mut cache = SourceCache::new(SourceCacheOptions::default());

    let source_han = query_named(&mut collection, &mut cache, SOURCE_HAN_NAMES);
    let jetbrains_mono = query_named(&mut collection, &mut cache, JETBRAINS_MONO_NAMES);
    let system_ui = query_fonts(
        &mut collection,
        &mut cache,
        [
            QueryFamily::Generic(GenericFamily::SystemUi),
            QueryFamily::Generic(GenericFamily::SansSerif),
        ],
    );
    let system_mono = query_fonts(
        &mut collection,
        &mut cache,
        [QueryFamily::Generic(GenericFamily::Monospace)],
    );

    let mut proportional = Vec::new();
    proportional.extend(system_ui);
    proportional.extend(source_han.clone());

    let mut monospace = Vec::new();
    monospace.extend(jetbrains_mono);
    monospace.extend(system_mono);
    monospace.extend(source_han);

    if proportional.is_empty() && monospace.is_empty() {
        tracing::debug!("未找到可预加载的系统字体，使用 egui 内置字体与系统缺字回退");
        return;
    }

    let mut definitions = FontDefinitions::default();
    prepend_fonts(&mut definitions, FontFamily::Proportional, proportional);
    prepend_fonts(&mut definitions, FontFamily::Monospace, monospace);
    ctx.set_fonts(definitions);
}

fn query_named(
    collection: &mut Collection,
    cache: &mut SourceCache,
    names: &[&str],
) -> Option<LoadedFont> {
    names.iter().find_map(|name| {
        query_fonts(collection, cache, [QueryFamily::Named(name)])
            .into_iter()
            .next()
    })
}

fn query_fonts<'a>(
    collection: &mut Collection,
    cache: &mut SourceCache,
    families: impl IntoIterator<Item = QueryFamily<'a>>,
) -> Vec<LoadedFont> {
    let mut selected = Vec::new();
    let mut seen_families = Vec::new();
    {
        let mut query = collection.query(cache);
        query.set_families(families);
        query.matches_with(|font| {
            if !seen_families.contains(&font.family.0) {
                seen_families.push(font.family.0);
                selected.push(font.clone());
            }
            QueryStatus::Continue
        });
    }

    selected
        .into_iter()
        .map(|font| {
            let family_name = collection.family_name(font.family.0).unwrap_or("unknown");
            let name = format!("system:{family_name}:{}:{}", font.family.1, font.index);
            let coords = VariationCoords::new(
                font.synthesis
                    .variation_settings()
                    .iter()
                    .map(|(tag, value)| ((*tag).into_bytes(), *value)),
            );
            tracing::debug!("系统字体已加载: {name}, 字重参数: {:?}", font.synthesis);
            let (blob, _) = font.blob.into_raw_parts();
            LoadedFont {
                name,
                data: FontData::from_blob(blob, font.index).tweak(FontTweak {
                    coords,
                    ..Default::default()
                }),
            }
        })
        .collect()
}

fn prepend_fonts(definitions: &mut FontDefinitions, family: FontFamily, fonts: Vec<LoadedFont>) {
    if fonts.is_empty() {
        return;
    }

    let names = fonts
        .iter()
        .map(|font| font.name.clone())
        .collect::<Vec<_>>();
    for font in fonts {
        definitions.font_data.insert(font.name, Arc::new(font.data));
    }
    definitions
        .families
        .entry(family)
        .or_default()
        .splice(0..0, names);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered_font() -> (Collection, SourceCache, String) {
        let definitions = FontDefinitions::default();
        let data = definitions.font_data["Ubuntu-Light"].clone();
        let mut collection = Collection::new(CollectionOptions {
            shared: false,
            system_fonts: false,
        });
        collection.register_fonts(fontique::Blob::new(data.font.clone()), None);
        let family = collection.family_names().next().unwrap().to_owned();
        (collection, SourceCache::default(), family)
    }

    #[test]
    fn named_query_skips_missing_fonts() {
        let (mut collection, mut cache, family) = registered_font();

        assert!(query_named(&mut collection, &mut cache, &["missing-font"]).is_none());
        let selected = query_named(&mut collection, &mut cache, &["missing-font", &family])
            .expect("已注册字体应作为后续候选被找到");
        assert!(selected.name.starts_with("system:"));
    }

    #[test]
    fn system_fonts_precede_builtin_fallbacks_per_family() {
        let mut definitions = FontDefinitions::default();
        let original = definitions.clone();
        let data = definitions.font_data["Ubuntu-Light"].as_ref().clone();
        let font = LoadedFont {
            name: "test-system-font".into(),
            data,
        };

        prepend_fonts(
            &mut definitions,
            FontFamily::Proportional,
            vec![font.clone()],
        );
        assert_eq!(
            definitions.families[&FontFamily::Proportional][0],
            "test-system-font"
        );
        assert_eq!(
            definitions.families[&FontFamily::Monospace],
            original.families[&FontFamily::Monospace]
        );

        prepend_fonts(&mut definitions, FontFamily::Monospace, vec![font]);
        assert_eq!(
            definitions.families[&FontFamily::Monospace][0],
            "test-system-font"
        );
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            assert!(
                definitions.families[&family]
                    .iter()
                    .any(|name| name == "egui-icons")
            );
        }
    }
}
