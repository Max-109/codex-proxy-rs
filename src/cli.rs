use crate::auth::AuthManager;
use crate::config::{ProxySettings, ReasoningEffort, SettingsManager, Speed, SystemMessages};
use crate::server::{ServerConfig, run_server};
use base64::Engine;
use clap::{Parser, Subcommand};
use inquire::{
    Confirm, Select, Text,
    error::InquireError,
    ui::{Color, ErrorMessageRenderConfig, RenderConfig, StyleSheet, Styled},
};
use rand::RngCore;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

const DEFAULT_PROXY_PORT: u16 = 8080;
const DEFAULT_PROXY_HOST: IpAddr = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));

#[derive(Debug, Parser)]
#[command(name = "codex-proxy")]
#[command(about = "OpenAI-compatible proxy backed by a Codex account")]
pub struct Cli {
    #[arg(long, global = true, value_name = "FILE")]
    pub auth_file: Option<PathBuf>,

    #[arg(long, global = true, value_name = "FILE")]
    pub settings_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(hide = true)]
    Menu,
    Login {
        #[command(subcommand)]
        method: LoginMethod,
    },
    Serve {
        #[arg(long, default_value_t = DEFAULT_PROXY_HOST)]
        host: IpAddr,

        #[arg(long, default_value_t = DEFAULT_PROXY_PORT)]
        port: u16,
    },
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Logout,
}

#[derive(Debug, Subcommand)]
enum LoginMethod {
    Browser,
    Device,
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    Status,
}

impl Cli {
    pub async fn run(self) -> anyhow::Result<()> {
        let auth_manager = AuthManager::new(
            self.auth_file
                .unwrap_or_else(AuthManager::default_auth_file),
        );
        let settings_manager = SettingsManager::new(
            self.settings_file
                .unwrap_or_else(SettingsManager::default_settings_file),
        );

        match self.command.unwrap_or(Command::Menu) {
            Command::Menu => run_menu(auth_manager, settings_manager).await?,
            Command::Login { method } => match method {
                LoginMethod::Browser => {
                    login_browser(&auth_manager).await?;
                    let settings = settings_manager.load()?;
                    start_proxy(auth_manager, settings.clone(), settings.host, settings.port)
                        .await?;
                }
                LoginMethod::Device => {
                    login_headless(&auth_manager).await?;
                    let settings = settings_manager.load()?;
                    start_proxy(auth_manager, settings.clone(), settings.host, settings.port)
                        .await?;
                }
            },
            Command::Serve { host, port } => {
                start_proxy(auth_manager, settings_manager.load()?, host, port).await?;
            }
            Command::Auth { command } => match command {
                AuthCommand::Status => print_auth_status(&auth_manager),
            },
            Command::Logout => {
                logout(&auth_manager)?;
            }
        }

        Ok(())
    }
}

async fn run_menu(
    auth_manager: AuthManager,
    settings_manager: SettingsManager,
) -> anyhow::Result<()> {
    loop {
        let settings = settings_manager.load()?;
        clear_terminal();
        print_menu_header(&auth_manager, &settings);

        match Select::new(
            "Action",
            vec![
                MenuAction::StartProxy,
                MenuAction::Login,
                MenuAction::Settings,
                MenuAction::Logout,
            ],
        )
        .with_render_config(fleety_render_config())
        .prompt()?
        {
            MenuAction::StartProxy => {
                start_proxy(auth_manager, settings.clone(), settings.host, settings.port).await?;
                return Ok(());
            }
            MenuAction::Login => {
                run_login_menu(&auth_manager).await?;
                let settings = settings_manager.load()?;
                start_proxy(auth_manager, settings.clone(), settings.host, settings.port).await?;
                return Ok(());
            }
            MenuAction::Settings => run_settings_menu(&auth_manager, &settings_manager)?,
            MenuAction::Logout => confirm_menu_logout(&auth_manager)?,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum MenuAction {
    StartProxy,
    Login,
    Settings,
    Logout,
}

impl std::fmt::Display for MenuAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StartProxy => write!(
                formatter,
                "Start proxy      Run the OpenAI-compatible endpoint"
            ),
            Self::Login => write!(formatter, "Login            Browser or headless sign-in"),
            Self::Settings => write!(formatter, "Settings         Endpoint, speed, and reasoning"),
            Self::Logout => write!(formatter, "Logout           Remove saved Codex auth"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum LoginAction {
    Browser,
    Headless,
}

impl std::fmt::Display for LoginAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Browser => write!(formatter, "Browser          Open the sign-in page locally"),
            Self::Headless => write!(formatter, "Headless         Device code for remote servers"),
        }
    }
}

async fn run_login_menu(auth_manager: &AuthManager) -> anyhow::Result<()> {
    print_section_header("Login");

    match Select::new("Method", vec![LoginAction::Browser, LoginAction::Headless])
        .with_render_config(fleety_render_config())
        .prompt()?
    {
        LoginAction::Browser => login_browser(auth_manager).await?,
        LoginAction::Headless => login_headless(auth_manager).await?,
    }

    Ok(())
}

fn run_settings_menu(
    auth_manager: &AuthManager,
    settings_manager: &SettingsManager,
) -> anyhow::Result<()> {
    let mut settings = settings_manager.load()?;
    let mut status_message = None;

    loop {
        clear_terminal();
        print_settings_header(auth_manager, settings_manager, &settings, status_message);

        let settings_action = match Select::new("Setting", settings_menu_items(&settings))
            .with_render_config(fleety_render_config())
            .prompt()
        {
            Ok(settings_menu_item) => settings_menu_item.action,
            Err(InquireError::OperationCanceled) => {
                clear_terminal();
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };

        match settings_action {
            SettingsAction::ReasoningEffort => {
                let selected_reasoning_effort = match Select::new(
                    "Reasoning effort",
                    vec![
                        ReasoningEffort::None,
                        ReasoningEffort::Minimal,
                        ReasoningEffort::Low,
                        ReasoningEffort::Medium,
                        ReasoningEffort::High,
                        ReasoningEffort::XHigh,
                    ],
                )
                .with_starting_cursor(reasoning_effort_index(settings.reasoning_effort))
                .with_render_config(fleety_render_config())
                .prompt()
                {
                    Ok(reasoning_effort) => reasoning_effort,
                    Err(InquireError::OperationCanceled) => continue,
                    Err(error) => return Err(error.into()),
                };
                settings.reasoning_effort = selected_reasoning_effort;
            }
            SettingsAction::Speed => {
                let selected_speed = match Select::new("Speed", vec![Speed::Normal, Speed::Fast])
                    .with_starting_cursor(speed_index(settings.speed))
                    .with_render_config(fleety_render_config())
                    .prompt()
                {
                    Ok(speed) => speed,
                    Err(InquireError::OperationCanceled) => continue,
                    Err(error) => return Err(error.into()),
                };
                settings.speed = selected_speed;
            }
            SettingsAction::SystemMessages => {
                let selected_system_messages = match Select::new(
                    "System messages",
                    vec![SystemMessages::PassThrough, SystemMessages::Ignore],
                )
                .with_starting_cursor(system_messages_index(settings.system_messages))
                .with_render_config(fleety_render_config())
                .prompt()
                {
                    Ok(system_messages) => system_messages,
                    Err(InquireError::OperationCanceled) => continue,
                    Err(error) => return Err(error.into()),
                };
                settings.system_messages = selected_system_messages;
            }
            SettingsAction::Host => {
                let Some(host) = prompt_host(settings.host)? else {
                    continue;
                };
                settings.host = host;
            }
            SettingsAction::Port => {
                let Some(port) = prompt_port(settings.port)? else {
                    continue;
                };
                settings.port = port;
            }
            SettingsAction::DetailedLogs => {
                let Some(detailed_logs) = prompt_detailed_logs(settings.detailed_logs)? else {
                    continue;
                };
                settings.detailed_logs = detailed_logs;
            }
            SettingsAction::ApiKeys => {
                let Some(api_key_status) = run_api_keys_menu(&mut settings)? else {
                    continue;
                };
                status_message = Some(api_key_status);
                settings_manager.save(&settings)?;
                continue;
            }
        }

        settings_manager.save(&settings)?;
        status_message = Some("Settings saved");
    }
}

#[derive(Clone, Copy, Debug)]
enum SettingsAction {
    ReasoningEffort,
    Speed,
    SystemMessages,
    Host,
    Port,
    DetailedLogs,
    ApiKeys,
}

#[derive(Clone, Debug)]
struct SettingsMenuItem {
    action: SettingsAction,
    label: String,
}

impl std::fmt::Display for SettingsMenuItem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.label)
    }
}

fn settings_menu_items(settings: &ProxySettings) -> Vec<SettingsMenuItem> {
    vec![
        SettingsMenuItem {
            action: SettingsAction::ReasoningEffort,
            label: format!("Reasoning       {}", settings.reasoning_effort),
        },
        SettingsMenuItem {
            action: SettingsAction::Speed,
            label: format!("Speed           {}", settings.speed),
        },
        SettingsMenuItem {
            action: SettingsAction::SystemMessages,
            label: format!("System messages {}", settings.system_messages),
        },
        SettingsMenuItem {
            action: SettingsAction::Host,
            label: format!("IP / host       {}", settings.host),
        },
        SettingsMenuItem {
            action: SettingsAction::Port,
            label: format!("Port            {}", settings.port),
        },
        SettingsMenuItem {
            action: SettingsAction::DetailedLogs,
            label: format!("Detailed logs   {}", enabled_label(settings.detailed_logs)),
        },
        SettingsMenuItem {
            action: SettingsAction::ApiKeys,
            label: format!("API keys        {}", settings.api_keys.len()),
        },
    ]
}

fn reasoning_effort_index(reasoning_effort: ReasoningEffort) -> usize {
    match reasoning_effort {
        ReasoningEffort::None => 0,
        ReasoningEffort::Minimal => 1,
        ReasoningEffort::Low => 2,
        ReasoningEffort::Medium => 3,
        ReasoningEffort::High => 4,
        ReasoningEffort::XHigh => 5,
    }
}

fn speed_index(speed: Speed) -> usize {
    match speed {
        Speed::Normal => 0,
        Speed::Fast => 1,
    }
}

fn system_messages_index(system_messages: SystemMessages) -> usize {
    match system_messages {
        SystemMessages::PassThrough => 0,
        SystemMessages::Ignore => 1,
    }
}

fn prompt_host(current_host: IpAddr) -> anyhow::Result<Option<IpAddr>> {
    let host_input = match Text::new("IP / host")
        .with_initial_value(&current_host.to_string())
        .with_render_config(fleety_render_config())
        .prompt()
    {
        Ok(host_input) => host_input,
        Err(InquireError::OperationCanceled) => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    Ok(Some(host_input.parse().map_err(|error| {
        anyhow::anyhow!("invalid IP address `{host_input}`: {error}")
    })?))
}

fn prompt_port(current_port: u16) -> anyhow::Result<Option<u16>> {
    let port_input = match Text::new("Port")
        .with_initial_value(&current_port.to_string())
        .with_render_config(fleety_render_config())
        .prompt()
    {
        Ok(port_input) => port_input,
        Err(InquireError::OperationCanceled) => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    Ok(Some(port_input.parse::<u16>().map_err(|error| {
        anyhow::anyhow!("invalid port `{port_input}`: {error}")
    })?))
}

fn prompt_detailed_logs(current_detailed_logs: bool) -> anyhow::Result<Option<bool>> {
    match Confirm::new("Detailed logs")
        .with_default(current_detailed_logs)
        .with_render_config(fleety_render_config())
        .prompt()
    {
        Ok(detailed_logs) => Ok(Some(detailed_logs)),
        Err(InquireError::OperationCanceled) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn run_api_keys_menu(settings: &mut ProxySettings) -> anyhow::Result<Option<&'static str>> {
    loop {
        clear_terminal();
        print_section_header("API Keys");
        print_field("Allowed", settings.api_keys.len());
        println!();

        let action = match Select::new("API keys", vec![ApiKeysAction::List, ApiKeysAction::Create])
            .with_render_config(fleety_render_config())
            .prompt()
        {
            Ok(action) => action,
            Err(InquireError::OperationCanceled) => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        match action {
            ApiKeysAction::List => {
                if let Some(deleted_key) = run_api_keys_list(settings)? {
                    settings.api_keys.retain(|api_key| api_key != &deleted_key);
                    return Ok(Some("API key deleted"));
                }
            }
            ApiKeysAction::Create => {
                let api_key = new_proxy_api_key();
                println!();
                print_field("New key", &api_key);
                settings.api_keys.push(api_key);
                wait_for_enter()?;
                return Ok(Some("API key created"));
            }
        }
    }
}

fn run_api_keys_list(settings: &ProxySettings) -> anyhow::Result<Option<String>> {
    if settings.api_keys.is_empty() {
        println!("No API keys created.");
        wait_for_enter()?;
        return Ok(None);
    }

    let selected_api_key = match Select::new("Allowed API key", settings.api_keys.clone())
        .with_render_config(fleety_render_config())
        .prompt()
    {
        Ok(api_key) => api_key,
        Err(InquireError::OperationCanceled) => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    let delete_action = match Select::new(
        "API key action",
        vec![ApiKeyDeleteAction::Delete, ApiKeyDeleteAction::Cancel],
    )
    .with_render_config(fleety_render_config())
    .prompt()
    {
        Ok(action) => action,
        Err(InquireError::OperationCanceled) => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    match delete_action {
        ApiKeyDeleteAction::Delete => Ok(Some(selected_api_key)),
        ApiKeyDeleteAction::Cancel => Ok(None),
    }
}

fn new_proxy_api_key() -> String {
    let mut random_bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut random_bytes);
    format!(
        "cp_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes)
    )
}

#[derive(Clone, Copy, Debug)]
enum ApiKeysAction {
    List,
    Create,
}

impl std::fmt::Display for ApiKeysAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::List => write!(formatter, "List"),
            Self::Create => write!(formatter, "Create"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ApiKeyDeleteAction {
    Delete,
    Cancel,
}

impl std::fmt::Display for ApiKeyDeleteAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Delete => write!(formatter, "Delete"),
            Self::Cancel => write!(formatter, "Cancel"),
        }
    }
}

async fn login_headless(auth_manager: &AuthManager) -> anyhow::Result<()> {
    let saved_auth = auth_manager.login_with_device_code().await?;
    print_login_success(auth_manager, saved_auth.account_id.as_deref());
    Ok(())
}

async fn login_browser(auth_manager: &AuthManager) -> anyhow::Result<()> {
    let saved_auth = auth_manager.login_with_browser().await?;
    print_login_success(auth_manager, saved_auth.account_id.as_deref());
    Ok(())
}

async fn start_proxy(
    auth_manager: AuthManager,
    settings: ProxySettings,
    host: IpAddr,
    port: u16,
) -> anyhow::Result<()> {
    print_starting_proxy(&settings, host, port);

    run_server(ServerConfig {
        host,
        port,
        auth_manager,
        settings,
    })
    .await
}

fn logout(auth_manager: &AuthManager) -> anyhow::Result<()> {
    if auth_manager.delete_auth()? {
        println!("Removed auth file");
        print_field("File", auth_manager.auth_file().display());
        return Ok(());
    }
    println!("No saved auth found");
    print_field("File", auth_manager.auth_file().display());
    Ok(())
}

fn confirm_menu_logout(auth_manager: &AuthManager) -> anyhow::Result<()> {
    print_section_header("Logout");
    print_field("File", auth_manager.auth_file().display());

    if !Confirm::new("Remove saved Codex auth?")
        .with_default(false)
        .with_render_config(fleety_render_config())
        .prompt()?
    {
        println!("Logout cancelled.");
        return Ok(());
    }

    logout(auth_manager)
}

fn print_auth_status(auth_manager: &AuthManager) {
    match auth_manager.load_saved_auth() {
        Ok(saved_auth) => {
            print_section_header("Auth");
            print_field("File", auth_manager.auth_file().display());
            print_field("Status", "signed in");
            print_field("Access token", "saved");
            print_field("Refresh token", "saved");
            print_field("Expires", saved_auth.expires_at_millis);
            print_field(
                "Account",
                saved_auth.account_id.as_deref().unwrap_or("unknown"),
            );
        }
        Err(error) => {
            print_section_header("Auth");
            print_field("File", auth_manager.auth_file().display());
            print_field("Status", error);
        }
    }
}

fn print_login_success(auth_manager: &AuthManager, account_id: Option<&str>) {
    println!("Login saved");
    print_field("File", auth_manager.auth_file().display());
    if let Some(account_id) = account_id {
        print_field("Account", account_id);
    }
}

fn print_menu_header(auth_manager: &AuthManager, settings: &ProxySettings) {
    print_section_header("Codex Proxy");
    print_field("Endpoint", endpoint(settings.host, settings.port));
    print_field("Auth", auth_status(auth_manager));
    print_field("Reasoning", settings.reasoning_effort);
    print_field("Speed", settings.speed);
    print_field("System messages", settings.system_messages);
    print_field("Logs", enabled_label(settings.detailed_logs));
    println!();
}

fn print_settings_header(
    auth_manager: &AuthManager,
    settings_manager: &SettingsManager,
    settings: &ProxySettings,
    status_message: Option<&str>,
) {
    print_section_header("Settings");
    print_field("Endpoint", endpoint(settings.host, settings.port));
    print_field("Auth", auth_status(auth_manager));
    print_field("File", settings_manager.settings_file().display());
    print_field("Logs", enabled_label(settings.detailed_logs));
    if let Some(status_message) = status_message {
        print_field("Status", status_message);
    }
    println!();
}

fn print_starting_proxy(settings: &ProxySettings, host: IpAddr, port: u16) {
    print_section_header("Starting Proxy");
    print_field("Endpoint", endpoint(host, port));
    print_field("Reasoning", settings.reasoning_effort);
    print_field("Speed", settings.speed);
    print_field("System messages", settings.system_messages);
    print_field("Logs", enabled_label(settings.detailed_logs));
    println!();
}

fn print_section_header(title: &str) {
    println!();
    println!("{}", paint(title, FleetyTerminalColor::Primary));
    println!(
        "{}",
        paint("-".repeat(title.len()), FleetyTerminalColor::Muted)
    );
}

fn print_field(label: &str, value: impl std::fmt::Display) {
    println!(
        "{} {}",
        paint(format!("{label:<10}"), FleetyTerminalColor::Muted),
        paint(value, FleetyTerminalColor::Secondary)
    );
}

fn endpoint(host: IpAddr, port: u16) -> String {
    format!("http://{host}:{port}/v1")
}

fn auth_status(auth_manager: &AuthManager) -> &'static str {
    if auth_manager.load_saved_auth().is_ok() {
        return "signed in";
    }

    "not signed in"
}

fn enabled_label(enabled: bool) -> &'static str {
    if enabled {
        return "enabled";
    }

    "disabled"
}

fn fleety_render_config() -> RenderConfig<'static> {
    RenderConfig::empty()
        .with_prompt_prefix(Styled::new("?").with_fg(fleety_blue()))
        .with_answered_prompt_prefix(Styled::new(">").with_fg(fleety_blue()))
        .with_help_message(StyleSheet::new().with_fg(fleety_muted()))
        .with_answer(StyleSheet::new().with_fg(fleety_primary()))
        .with_error_message(
            ErrorMessageRenderConfig::empty()
                .with_prefix(Styled::new("#").with_fg(fleety_red()))
                .with_message(StyleSheet::new().with_fg(fleety_red())),
        )
        .with_highlighted_option_prefix(Styled::new(">").with_fg(fleety_blue()))
        .with_scroll_up_prefix(Styled::new("^").with_fg(fleety_muted()))
        .with_scroll_down_prefix(Styled::new("v").with_fg(fleety_muted()))
        .with_option(StyleSheet::new().with_fg(fleety_secondary()))
        .with_selected_option(Some(StyleSheet::new().with_fg(fleety_primary())))
        .with_text_input(StyleSheet::new().with_fg(fleety_primary()))
        .with_default_value(StyleSheet::new().with_fg(fleety_muted()))
}

fn fleety_primary() -> Color {
    Color::AnsiValue(255)
}

fn fleety_secondary() -> Color {
    Color::AnsiValue(250)
}

fn fleety_muted() -> Color {
    Color::AnsiValue(245)
}

fn fleety_blue() -> Color {
    Color::AnsiValue(68)
}

fn fleety_red() -> Color {
    Color::AnsiValue(167)
}

enum FleetyTerminalColor {
    Primary,
    Secondary,
    Muted,
}

fn paint(value: impl std::fmt::Display, color: FleetyTerminalColor) -> String {
    let (red, green, blue) = match color {
        FleetyTerminalColor::Primary => (244, 244, 244),
        FleetyTerminalColor::Secondary => (184, 184, 184),
        FleetyTerminalColor::Muted => (133, 133, 133),
    };

    format!("\x1b[38;2;{red};{green};{blue}m{value}\x1b[0m")
}

fn clear_terminal() {
    print!("\x1b[2J\x1b[H");
    let _ = io::stdout().flush();
}

fn wait_for_enter() -> anyhow::Result<()> {
    print!(
        "{}",
        paint("Press Enter to continue...", FleetyTerminalColor::Muted)
    );
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(())
}
