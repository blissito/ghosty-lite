pub mod configure;
pub mod doctor;
pub mod info;
pub mod plugin;
pub mod recipe;
pub mod review;
pub mod schedule;
pub mod serve_setup;
pub mod session;
pub mod skills;
pub mod term;

/// Un sí/no en español. cliclack 0.5 no deja cambiar el "Yes / No" de `confirm`,
/// así que es un `select` de dos opciones con la misma cadena de llamadas
/// (`.initial_value(bool).interact() -> bool`).
pub fn confirm_es(prompt: impl std::fmt::Display) -> cliclack::Select<bool> {
    cliclack::select(prompt)
        .item(true, "Sí", "")
        .item(false, "No", "")
}
