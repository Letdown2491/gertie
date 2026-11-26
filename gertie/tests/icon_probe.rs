use gpui_kit::assets::__private::AssetSource as _;

#[test]
fn the_asset_source_loads_every_icon_the_ui_draws() {
    let source = gpui_kit::assets::AllAssets;
    for name in [
        "icons/square-pen.svg",
        "icons/search.svg",
        "icons/git-branch-plus.svg",
        "icons/git-fork.svg",
        "icons/copy.svg",
        "icons/settings.svg",
        "icons/undo-2.svg",
        "icons/text-quote.svg",
        "icons/pin.svg",
    ] {
        match source.load(name) {
            Ok(Some(bytes)) => assert!(!bytes.is_empty(), "{name} empty"),
            other => panic!("{name} failed to load: {other:?}"),
        }
    }
}
