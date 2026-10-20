use crate::core::error::AppError;

/// Use portable process-environment names. Do not echo a submitted name or value in errors.
pub fn validate_env_names<'a>(names: impl Iterator<Item = &'a str>) -> Result<(), AppError> {
    for name in names {
        let mut bytes = name.bytes();
        if !bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(AppError::Validation(
                "Environment variable names must start with a letter or underscore and contain only ASCII letters, digits, and underscores".into(),
            ));
        }
    }
    Ok(())
}
