use async_openai::{Client, config::OpenAIConfig, error::OpenAIError};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{self, Write},
    path::PathBuf,
};
use strum_macros::Display;

use crate::save::get_game_data_dir;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Settings {
    pub language: Language,
    pub openai_api_key: Option<String>,
    // TODO: Make the model an enum
    pub model: String,
    // TODO: Make the audio an enum
    pub audio_output_enabled: bool,
    pub audio_input_enabled: bool,
    pub debug_mode: bool,
}

#[derive(PartialEq, Eq, Debug, Serialize, Deserialize, Clone, Default, Display)]
pub enum Language {
    #[default]
    English,
    French,
    Japanese,
    Turkish,
    Custom(String),
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, Display)]
pub enum Model {
    #[default]
    Gpt4oMini,
    Gpt4o,
}

// TODO:  Add a model parameter to change the AI model

impl Default for Settings {
    fn default() -> Self {
        Settings {
            language: Language::English,
            openai_api_key: None,
            model: "gpt-4o-mini".to_string(),
            audio_output_enabled: false,
            audio_input_enabled: false,
            debug_mode: true,
        }
    }
}

impl Settings {
    /// Load settings from the default location, with environment variable override support.
    ///
    /// This method first loads settings from the settings.json file, then checks for the
    /// OPENAI_API_KEY environment variable. If the environment variable is set and non-empty,
    /// it will override any API key found in the settings file.
    ///
    /// Environment variable precedence:
    /// 1. OPENAI_API_KEY environment variable (highest priority)
    /// 2. openai_api_key from settings.json file
    /// 3. Default settings (no API key)
    pub fn try_load() -> Self {
        let path = get_game_data_dir().join("settings.json");
        let mut settings = Self::load_settings_from_file(path).unwrap_or_default();
        if let Ok(env_api_key) = env::var("OPENAI_API_KEY") {
            if !env_api_key.is_empty() {
                settings.openai_api_key = Some(env_api_key);
                log::info!("Using OpenAI API key from environment variable");
            }
        }

        settings
    }

    // Load settings from a specified file path.
    pub fn load_settings_from_file(path: PathBuf) -> io::Result<Self> {
        let data = fs::read_to_string(path)?; // Read settings from file.
        let settings = serde_json::from_str(&data)?; // Deserialize JSON data into settings.
        Ok(settings)
    }

    // Save current settings to a specified file path.
    pub fn save_to_file(&self, path: PathBuf) -> io::Result<()> {
        let data = serde_json::to_string_pretty(self)?; // Serialize settings into pretty JSON format.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?; // Create the directory if it doesn't exist.
        }
        let mut file = fs::File::create(path)?; // Create or overwrite the file.
        file.write_all(data.as_bytes())?; // Write the serialized data to the file.
        Ok(())
    }

    pub fn save(&self) -> io::Result<()> {
        let data = serde_json::to_string_pretty(self)?; // Serialize settings into pretty JSON format.
        let path = get_game_data_dir().join("settings.json");
        let mut file = fs::File::create(path)?; // Create or overwrite the file.
        file.write_all(data.as_bytes())?; // Write the serialized data to the file.
        Ok(())
    }

    // Asynchronously validate an API key with OpenAI's services.
    pub async fn validate_ai_client(api_key: &str) -> Option<Client<OpenAIConfig>> {
        let client = Client::with_config(OpenAIConfig::new().with_api_key(api_key)); // Configure the OpenAI client with the API key.
        let maybe_client = match client.models().list().await {
            Ok(_) => Some(client),
            Err(OpenAIError::Reqwest(_)) => None,
            _ => None,
        };
        log::debug!("validate_ai_client: {maybe_client:#?}");
        maybe_client
    }
}
