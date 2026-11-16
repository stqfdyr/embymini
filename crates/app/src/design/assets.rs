//! Icons beyond the set the components embed: only the ones the pages use, so the binary carries no unused SVGs.

use gpui_kit::{AssetSource, Result, SharedString};
use std::borrow::Cow;

gpui_kit::assets::icon_assets!(
    PageIcons,
    [
        ArrowDownWideNarrow,
        Captions,
        Check,
        ChevronDown,
        ChevronLeft,
        ChevronRight,
        ChevronsUpDown,
        CircleCheck,
        Clapperboard,
        Ellipsis,
        HardDrive,
        Heart,
        House,
        Info,
        LayoutGrid,
        List,
        MonitorPlay,
        PanelLeft,
        Moon,
        Music,
        Palette,
        Play,
        Plus,
        RefreshCw,
        Search,
        Server,
        Settings,
        Star,
        Sun,
        Tv,
    ]
);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Ok(Some(bytes)) = PageIcons.load(path) {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(PageIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
