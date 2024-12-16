mod commands {
    pub mod configure;
    pub mod session;
    pub mod version;
}
pub mod agents;
mod profile;
mod prompt;
mod langfuse_layer;
pub mod session;

mod systems;

use anyhow::Result;
use clap::{Parser, Subcommand};
use commands::configure::handle_configure;
use commands::session::build_session;
use commands::version::print_version;
use profile::has_no_profiles;
use std::io::{self, Read};
use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};
use tracing_subscriber::util::SubscriberInitExt;  // Add this import
use std::fs::{OpenOptions};
use chrono::Local;

#[cfg(test)]
mod test_helpers;

use crate::systems::system_handler::{add_system, remove_system};

#[derive(Parser)]
#[command(author, about, long_about = None)]
struct Cli {
    #[arg(short = 'v', long = "version")]
    version: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Configure Goose settings and profiles
    #[command(about = "Configure Goose settings and profiles")]
    Configure {
        /// Name of the profile to configure
        #[arg(
            short('n'),
            long,
            help = "Profile name to configure",
            long_help = "Create or modify a named configuration profile. Use 'default' for the default profile."
        )]
        profile_name: Option<String>,

        /// AI Provider to use
        #[arg(
            short,
            long,
            help = "AI Provider to use (e.g., 'openai', 'databricks', 'ollama')",
            long_help = "Specify AI Provider to use (e.g., 'openai', 'databricks', 'ollama')."
        )]
        provider: Option<String>,

        /// Model to use
        #[arg(
            short,
            long,
            help = "Model to use (e.g., 'gpt-4', 'llama2')",
            long_help = "Specify which model to use for this profile."
        )]
        model: Option<String>,
    },

    /// Manage system prompts and behaviors
    #[command(about = "Manage the systems that goose can operate")]
    System {
        #[command(subcommand)]
        action: SystemCommands,
    },

    /// Start or resume interactive chat sessions
    #[command(about = "Start or resume interactive chat sessions", alias = "s")]
    Session {
        /// Name for the chat session
        #[arg(
            short,
            long,
            value_name = "NAME",
            help = "Name for the chat session (e.g., 'project-x')",
            long_help = "Specify a name for your chat session. When used with --resume, will resume this specific session if it exists."
        )]
        name: Option<String>,

        /// Configuration profile to use
        #[arg(
            short,
            long,
            value_name = "PROFILE",
            help = "Configuration profile to use (e.g., 'default')",
            long_help = "Use a specific configuration profile. Profiles contain settings like API keys and model preferences."
        )]
        profile: Option<String>,

        /// Resume a previous session
        #[arg(
            short,
            long,
            help = "Resume a previous session (last used or specified by --session)",
            long_help = "Continue from a previous chat session. If --session is provided, resumes that specific session. Otherwise resumes the last used session."
        )]
        resume: bool,
    },

    /// Execute commands from an instruction file
    #[command(about = "Execute commands from an instruction file or stdin")]
    Run {
        /// Path to instruction file containing commands
        #[arg(
            short,
            long,
            required = true,
            value_name = "FILE",
            help = "Path to instruction file containing commands"
        )]
        instructions: Option<String>,

        /// Configuration profile to use
        #[arg(
            short,
            long,
            value_name = "PROFILE",
            help = "Configuration profile to use (e.g., 'default')",
            long_help = "Use a specific configuration profile. Profiles contain settings like API keys and model preferences."
        )]
        profile: Option<String>,

        /// Input text containing commands
        #[arg(
            short = 't',
            long = "text",
            value_name = "TEXT",
            help = "Input text to provide to Goose directly",
            long_help = "Input text containing commands for Goose. Use this in lieu of the instructions argument."
        )]
        input_text: Option<String>,

        /// Name for this run session
        #[arg(
            short,
            long,
            value_name = "NAME",
            help = "Name for this run session (e.g., 'daily-tasks')",
            long_help = "Specify a name for this run session. This helps identify and resume specific runs later."
        )]
        name: Option<String>,

        /// Resume a previous run
        #[arg(
            short,
            long,
            action = clap::ArgAction::SetTrue,
            help = "Resume from a previous run",
            long_help = "Continue from a previous run, maintaining the execution state and context."
        )]
        resume: bool,
    },
}

#[derive(Subcommand)]
enum SystemCommands {
    /// Add a new system prompt
    #[command(about = "Add a new system prompt from URL")]
    Add {
        #[arg(
            help = "URL of the system prompt to add",
            long_help = "URL pointing to a file containing the system prompt to be added."
        )]
        url: String,
    },

    /// Remove an existing system prompt
    #[command(about = "Remove an existing system prompt")]
    Remove {
        #[arg(
            help = "URL of the system prompt to remove",
            long_help = "URL of the system prompt that should be removed from the configuration."
        )]
        url: String,
    },
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum CliProviderVariant {
    OpenAi,
    Databricks,
    Ollama,
}

fn setup_logging() -> Result<()> {
    // Create logs directory if it doesn't exist
    let home_dir = dirs::home_dir().ok_or(anyhow::anyhow!("Could not determine home directory"))?;
    let log_dir = home_dir.join(".config").join("goose").join("logs");
    std::fs::create_dir_all(&log_dir)?;

    // Create log file with timestamp
    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let log_file = log_dir.join(format!("goose_{}.log", timestamp));

    // Create file logging layer with detailed information
    let file_layer = fmt::layer()
        .with_writer(move || -> Box<dyn std::io::Write> {
            let file = OpenOptions::new()
                .write(true)
                .append(true)
                .create(true)
                .open(&log_file)
                .expect("Failed to open log file");
            Box::new(file)
        })
        .with_target(true)
        .with_thread_ids(true)
        .with_line_number(true)
        .with_file(true)
        .with_ansi(false);

    // Create the env filter
    let env_filter = EnvFilter::from_default_env()
        .add_directive(tracing::Level::INFO.into())
        .add_directive("agent_tracing=debug".parse()?);
        // .add_directive("goose=debug".parse()?);
        

    // Build the subscriber
    // let subscriber = tracing_subscriber::registry()
    //     .with(env_filter)
    //     .with(file_layer);

    // Add Langfuse layer if environment variables are present
    if let Some(langfuse_layer) = langfuse_layer::create_langfuse_layer() {
        let subscriber = tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .with(langfuse_layer).init();
    } else {
        println!("ENV VARIABLES ARE NOT SET!");
        // let subscriber = tracing_subscriber::registry()
        // .with(env_filter)
        // .with(file_layer)
        // .init();
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    if let Err(e) = setup_logging() {
        eprintln!("Failed to initialize logging: {}", e);
    }

    let cli = Cli::parse();

    if cli.version {
        print_version();
        return Ok(());
    }

    match cli.command {
        Some(Command::Configure {
            profile_name,
            provider,
            model,
        }) => {
            let _ = handle_configure(profile_name, provider, model).await;
            return Ok(());
        }
        Some(Command::System { action }) => match action {
            SystemCommands::Add { url } => {
                add_system(url).await.unwrap();
                return Ok(());
            }
            SystemCommands::Remove { url } => {
                remove_system(url).await.unwrap();
                return Ok(());
            }
        },
        Some(Command::Session {
            name,
            profile,
            resume,
        }) => {
            let mut session = build_session(name, profile, resume);
            let _ = session.start().await;
            return Ok(());
        }
        Some(Command::Run {
            instructions,
            input_text,
            profile,
            name,
            resume,
        }) => {
            let contents = if let Some(file_name) = instructions {
                let file_path = std::path::Path::new(&file_name);
                std::fs::read_to_string(file_path).expect("Failed to read the instruction file")
            } else if let Some(input_text) = input_text {
                input_text
            } else {
                let mut stdin = String::new();
                io::stdin()
                    .read_to_string(&mut stdin)
                    .expect("Failed to read from stdin");
                stdin
            };
            let mut session = build_session(name, profile, resume);
            let _ = session.headless_start(contents.clone()).await;
            return Ok(());
        }
        None => {
            println!("No command provided - Run 'goose help' to see available commands.");
            if has_no_profiles().unwrap_or(false) {
                println!("\nRun 'goose configure' to setup goose for the first time.");
            }
        }
    }
    Ok(())
}