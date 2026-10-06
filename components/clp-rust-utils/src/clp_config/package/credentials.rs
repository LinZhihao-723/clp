use secrecy::SecretString;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Credentials {
    pub database: Database,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Database {
    pub password: SecretString,
    pub user: String,
}

impl Database {
    /// Factory function.
    ///
    /// Reads the CLP database credentials from the `CLP_DB_USER` and `CLP_DB_PASS` environment
    /// variables.
    ///
    /// # Returns
    ///
    /// The database credentials on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`std::env::var`]'s return values on failure.
    pub fn from_env() -> Result<Self, std::env::VarError> {
        let password = std::env::var("CLP_DB_PASS").inspect_err(|e| {
            tracing::error!(
                error = % e,
                "Failed to read the database password from `CLP_DB_PASS`."
            );
        })?;
        let user = std::env::var("CLP_DB_USER").inspect_err(|e| {
            tracing::error!(
                error = % e,
                "Failed to read the database user from `CLP_DB_USER`."
            );
        })?;
        Ok(Self {
            password: SecretString::new(password.into_boxed_str()),
            user,
        })
    }
}
