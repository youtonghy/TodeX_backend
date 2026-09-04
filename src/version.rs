pub(crate) const APP_VERSION: &str = match option_env!("TODEX_BUILD_VERSION") {
    Some(version) => version,
    None => "DEV0.0.0",
};
