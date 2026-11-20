//! A mod's whole menu: build it once from a worker thread and keep it in step with config reloads.

use std::sync::OnceLock;
use std::time::Duration;

use super::abi::*;
use super::host::{Provider, Registration, connect};
use super::settings::{ColorSetting, NumberSetting, Setting, TextSetting, setting_changed};
use crate::logger;

type ByteReader = fn() -> u8;
type NumberReader = fn() -> f64;
type TextReader = fn() -> String;
/// Reads a color (`0xRRGGBB`) from the config by the `id` of its [`ColorSetting`].
type ColorReader = fn(usize) -> u32;

/// A committed menu: every row with the function that reads its value from the
/// mod's config, so [`Menu::sync`] can show a reloaded config.
pub struct Menu {
    registration: Registration,
    rows: Vec<(RowHandle, ByteReader)>,
    numbers: Vec<(&'static NumberSetting, NumberReader)>,
    colors: Vec<(&'static ColorSetting, ColorReader)>,
    texts: Vec<(&'static TextSetting, TextReader)>,
}

impl Menu {
    /// Shows the current config values in every row (call after a config reload).
    pub fn sync(&self) {
        for (row, read) in &self.rows {
            if let Err(reason) = self.registration.set_row_value(*row, read()) {
                logger::log(&format!("Menu: row update failed ({reason})"));
            }
        }
        for (setting, read) in &self.numbers {
            if let Err(reason) = setting.show(&self.registration, read()) {
                logger::log(&format!("Menu: row update failed ({reason})"));
            }
        }
        for (setting, read) in &self.texts {
            if let Err(reason) = setting.show(&self.registration, &read()) {
                logger::log(&format!("Menu: row update failed ({reason})"));
            }
        }
        for (setting, read) in &self.colors {
            if let Err(reason) = setting.show(&self.registration, read(setting.id)) {
                logger::log(&format!("Menu: row update failed ({reason})"));
            }
        }
    }
}

/// [`Menu::sync`] on a menu that may not exist (no host).
pub fn sync(slot: &OnceLock<Menu>) {
    if let Some(menu) = slot.get() {
        menu.sync();
    }
}

pub struct MenuBuilder {
    provider: Provider,
    rows: Vec<(RowHandle, ByteReader)>,
    numbers: Vec<(&'static NumberSetting, NumberReader)>,
    colors: Vec<(&'static ColorSetting, ColorReader)>,
    texts: Vec<(&'static TextSetting, TextReader)>,
}

impl MenuBuilder {
    /// The mod's row on System > Game Options is the parent of its top page.
    pub fn root(&self) -> PageHandle {
        self.provider.root_page()
    }

    /// The underlying provider, for a row the helpers below do not cover.
    pub fn provider(&self) -> &Provider {
        &self.provider
    }

    /// A row on `parent` that opens a page titled `name`; returns that page.
    pub fn submenu(
        &self,
        parent: PageHandle,
        name: &str,
        help: &str,
    ) -> Result<PageHandle, String> {
        Ok(self.provider.add_submenu(parent, name, help, name, help)?.0)
    }

    /// An on/off row; `read` gives its current value (`0` or `1`) from the config.
    pub fn toggle(
        &mut self,
        page: PageHandle,
        label: &str,
        help: &str,
        setting: &'static Setting,
        read: ByteReader,
    ) -> Result<(), String> {
        let row = self.provider.add_toggle(
            page,
            label,
            help,
            read() != 0,
            setting_changed,
            setting.user_data(),
        )?;
        self.rows.push((row, read));
        Ok(())
    }

    /// A left/right row over `options` (1 to 32 labels); the value is the index.
    pub fn choice(
        &mut self,
        page: PageHandle,
        label: &str,
        help: &str,
        options: &[&str],
        setting: &'static Setting,
        read: ByteReader,
    ) -> Result<(), String> {
        let row = self.provider.add_inline_choice(
            page,
            label,
            help,
            options,
            read(),
            setting_changed,
            setting.user_data(),
        )?;
        self.rows.push((row, read));
        Ok(())
    }

    /// A slider over the whole numbers `min..=max` (step 1, at most 255 positions).
    pub fn slider(
        &mut self,
        page: PageHandle,
        label: &str,
        help: &str,
        range: (i32, i32),
        setting: &'static Setting,
        read: ByteReader,
    ) -> Result<(), String> {
        let row = self.provider.add_slider(
            page,
            label,
            help,
            range.0,
            range.1,
            1,
            read(),
            setting_changed,
            setting.user_data(),
        )?;
        self.rows.push((row, read));
        Ok(())
    }

    /// A text row for a number (see [`NumberSetting`]); `read` gives the config's value.
    pub fn number(
        &mut self,
        page: PageHandle,
        label: &str,
        help: &str,
        setting: &'static NumberSetting,
        read: NumberReader,
    ) -> Result<(), String> {
        self.provider.add_number(page, label, help, setting, read())?;
        self.numbers.push((setting, read));
        Ok(())
    }

    /// A text row for a short name (see [`TextSetting`]); `read` gives the config's text.
    pub fn text(
        &mut self,
        page: PageHandle,
        label: &str,
        help: &str,
        setting: &'static TextSetting,
        read: TextReader,
    ) -> Result<(), String> {
        self.provider.add_text(page, label, help, setting, &read())?;
        self.texts.push((setting, read));
        Ok(())
    }

    /// A color row with the game's color picker; `read` gives the config's color for the `id` of
    /// `setting`. A host without color pickers gets no row (the config file still has the color).
    pub fn color(
        &mut self,
        page: PageHandle,
        label: &str,
        help: &str,
        setting: &'static ColorSetting,
        read: ColorReader,
    ) -> Result<(), String> {
        if !self.provider.color_picker {
            return Ok(());
        }
        self.provider.add_color(page, label, help, setting, read(setting.id))?;
        self.colors.push((setting, read));
        Ok(())
    }
}

/// Registers a mod's menu on a worker thread and stores it in `slot` once the host has installed
/// it. Call it very early: the host only accepts registrations in its first seconds. Without the
/// host it logs one line and the mod runs from its config file. `build` adds the rows.
pub fn spawn_menu(
    slot: &'static OnceLock<Menu>,
    hmodule: u64,
    provider_id: &'static str,
    display_name: &'static str,
    build: fn(&mut MenuBuilder) -> Result<(), String>,
) {
    std::thread::spawn(move || {
        let connection = match connect(Duration::from_secs(10)) {
            Ok(connection) => connection,
            Err(reason) => {
                logger::log(&format!("Menu: unavailable ({reason}), config file only"));
                return;
            }
        };
        let result = (|| {
            let provider = connection.register_provider(provider_id, display_name, hmodule)?;
            let mut builder = MenuBuilder {
                provider,
                rows: Vec::new(),
                numbers: Vec::new(),
                colors: Vec::new(),
                texts: Vec::new(),
            };
            build(&mut builder)?;
            let MenuBuilder { provider, rows, numbers, colors, texts } = builder;
            let registration = provider.commit()?;
            Ok::<_, String>(Menu { registration, rows, numbers, colors, texts })
        })();
        match result {
            Ok(menu) => {
                let _ = slot.set(menu);
                logger::log("Menu: registered");
            }
            Err(reason) => logger::log(&format!("Menu: registration failed ({reason})")),
        }
    });
}
