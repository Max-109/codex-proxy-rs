use crate::auth::AuthManager;
use crate::config::{ProxySettings, ReasoningEffort, SettingsManager, Speed};
use crate::server::{ServerConfig, run_server};
use clap::{Parser, Subcommand};
use inquire::{Confirm, Select, Text};
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
    let settings = settings_manager.load()?;
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
    .prompt()?
    {
        MenuAction::StartProxy => {
            start_proxy(auth_manager, settings.clone(), settings.host, settings.port).await?;
        }
        MenuAction::Login => {
            run_login_menu(&auth_manager).await?;
            let settings = settings_manager.load()?;
            start_proxy(auth_manager, settings.clone(), settings.host, settings.port).await?;
        }
        MenuAction::Settings => run_settings_menu(&auth_manager, &settings_manager)?,
        MenuAction::Logout => confirm_menu_logout(&auth_manager)?,
    }

    Ok(())
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

    match Select::new("Method", vec![LoginAction::Browser, LoginAction::Headless]).prompt()? {
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

    loop {
        print_settings_header(auth_manager, settings_manager, &settings);

        match Select::new("Setting", settings_menu_items(&settings))
            .prompt()?
            .action
        {
            SettingsAction::ReasoningEffort => {
                settings.reasoning_effort = Select::new(
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
                .prompt()?;
            }
            SettingsAction::Speed => {
                settings.speed = Select::new("Speed", vec![Speed::Normal, Speed::Fast])
                    .with_starting_cursor(speed_index(settings.speed))
                    .prompt()?;
            }
            SettingsAction::Host => {
                settings.host = prompt_host(settings.host)?;
            }
            SettingsAction::Port => {
                settings.port = prompt_port(settings.port)?;
            }
            SettingsAction::Back => return Ok(()),
        }

        settings_manager.save(&settings)?;
        print_settings_saved(settings_manager, &settings);
    }
}

#[derive(Clone, Copy, Debug)]
enum SettingsAction {
    ReasoningEffort,
    Speed,
    Host,
    Port,
    Back,
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
            action: SettingsAction::Host,
            label: format!("IP / host       {}", settings.host),
        },
        SettingsMenuItem {
            action: SettingsAction::Port,
            label: format!("Port            {}", settings.port),
        },
        SettingsMenuItem {
            action: SettingsAction::Back,
            label: "Back".to_string(),
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

fn prompt_host(current_host: IpAddr) -> anyhow::Result<IpAddr> {
    let host_input = Text::new("IP / host")
        .with_initial_value(&current_host.to_string())
        .prompt()?;
    host_input
        .parse()
        .map_err(|error| anyhow::anyhow!("invalid IP address `{host_input}`: {error}"))
}

fn prompt_port(current_port: u16) -> anyhow::Result<u16> {
    let port_input = Text::new("Port")
        .with_initial_value(&current_port.to_string())
        .prompt()?;
    port_input
        .parse::<u16>()
        .map_err(|error| anyhow::anyhow!("invalid port `{port_input}`: {error}"))
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

fn print_settings_saved(settings_manager: &SettingsManager, settings: &ProxySettings) {
    println!("Settings saved");
    print_field("File", settings_manager.settings_file().display());
    print_field("Endpoint", endpoint(settings.host, settings.port));
    print_field("Reasoning", settings.reasoning_effort);
    print_field("Speed", settings.speed);
}

fn print_menu_header(auth_manager: &AuthManager, settings: &ProxySettings) {
    print_section_header("Codex Proxy");
    print_field("Endpoint", endpoint(settings.host, settings.port));
    print_field("Auth", auth_status(auth_manager));
    print_field("Reasoning", settings.reasoning_effort);
    print_field("Speed", settings.speed);
    println!();
}

fn print_settings_header(
    auth_manager: &AuthManager,
    settings_manager: &SettingsManager,
    settings: &ProxySettings,
) {
    print_section_header("Settings");
    print_field("Endpoint", endpoint(settings.host, settings.port));
    print_field("Auth", auth_status(auth_manager));
    print_field("File", settings_manager.settings_file().display());
    println!();
}

fn print_starting_proxy(settings: &ProxySettings, host: IpAddr, port: u16) {
    print_section_header("Starting Proxy");
    print_field("Endpoint", endpoint(host, port));
    print_field("Reasoning", settings.reasoning_effort);
    print_field("Speed", settings.speed);
    println!();
}

fn print_section_header(title: &str) {
    println!();
    println!("{title}");
    println!("{}", "-".repeat(title.len()));
}

fn print_field(label: &str, value: impl std::fmt::Display) {
    println!("{label:<10} {value}");
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
