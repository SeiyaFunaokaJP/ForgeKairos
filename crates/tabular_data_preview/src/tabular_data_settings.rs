use settings::{RegisterSetting, Settings};

/// CSV / TSV などの表形式ファイルの開き方の設定。
#[derive(Clone, Copy, Debug, Default, RegisterSetting)]
pub struct TabularDataSettings {
    /// 表形式のファイルを表 (グリッド) で開くかどうか。
    pub open_as_grid: bool,
    /// 改行を含むセルを、行の高さを広げてすべて表示するかどうか。
    pub multiline_cells: bool,
}

impl Settings for TabularDataSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let content = content.tabular_data.clone().unwrap_or_default();
        Self {
            open_as_grid: content.open_as_grid.unwrap_or(true),
            multiline_cells: content.multiline_cells.unwrap_or(true),
        }
    }
}
