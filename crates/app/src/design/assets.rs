//! Icons beyond the set the components embed: only the ones the pages use, so the binary carries no unused SVGs.

use gpui_kit::{AssetSource, Result, SharedString};
use std::borrow::Cow;

gpui_kit::assets::icon_assets!(
    PageIcons,
    [
        ArrowDownUp,
        ArrowDownWideNarrow,
        ArrowLeftToLine,
        ArrowRightToLine,
        Captions,
        Check,
        ChevronDown,
        ChevronLeft,
        ChevronRight,
        ChevronsUpDown,
        CircleCheck,
        CirclePlay,
        Clapperboard,
        Ellipsis,
        Eye,
        Film,
        Folder,
        HardDrive,
        Headphones,
        Heart,
        House,
        Info,
        LayoutGrid,
        LibraryBig,
        Link,
        List,
        MonitorPlay,
        MessageSquareText,
        Moon,
        Music,
        Palette,
        PanelLeft,
        Play,
        Plus,
        RefreshCw,
        RotateCcw,
        Search,
        Server,
        Settings,
        Star,
        Sun,
        Trash,
        Tv,
        User,
        Video,
        Volume2,
        X,
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
