use super::parse::{Choice, ConfigType};
use crate::{
    config::{LIBEXECDIR, SYSCONFDIR},
    ui::{
        pages::{
            install::{InstallMsg, INSTALL_BROKER},
            partitions::PartitionSchema,
        },
        window::{AppMsg, UserConfig},
    },
};
use anyhow::{anyhow, Context, Result};
use log::{debug, error, info};
use relm4::*;
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

pub struct InstallAsyncModel {
    username: Option<String>,
    password: Option<String>,
    rootpassword: Option<String>,
}

#[derive(Debug)]
pub enum InstallAsyncMsg {
    Install(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Box<Option<PartitionSchema>>,
        Box<Option<UserConfig>>,
        HashMap<String, HashMap<String, Choice>>, // Listconfig
        ConfigType,
    ),
    FinishInstall,
}

impl Worker for InstallAsyncModel {
    type Init = ();
    type Input = InstallAsyncMsg;
    type Output = AppMsg;

    fn init(_parent_window: Self::Init, _sender: ComponentSender<Self>) -> Self {
        InstallAsyncModel {
            username: None,
            password: None,
            rootpassword: None,
        }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            InstallAsyncMsg::Install(
                id,
                language,
                timezone,
                keyboard,
                partitions,
                user,
                listconfig,
                configtype,
            ) => {
                self.username = user.as_ref().as_ref().map(|u| u.username.clone());
                self.password = user.as_ref().as_ref().map(|u| u.password.clone());
                self.rootpassword = user.as_ref().as_ref().and_then(|u| u.rootpassword.clone());
                let hostname = user.as_ref().as_ref().map(|u| u.hostname.clone()).unwrap_or_else(|| "nixos".to_string());
                let archout = match Command::new("uname")
                    .arg("-m")
                    .output()
                    .context("Failed to get architecture")
                {
                    Ok(o) => o,
                    Err(e) => {
                        error!("Failed to get architecture: {}", e);
                        let _ = sender.output(AppMsg::Error);
                        return;
                    }
                };
                let arch = String::from_utf8_lossy(&archout.stdout).trim().to_string();

                // Step 0: Clear /tmp/icicle
                info!("Step 0: Clear /tmp/icicle");
                fn clear() -> Result<()> {
                    Command::new("pkexec")
                        .arg("umount")
                        .arg("-R")
                        .arg("/tmp/icicle")
                        .output()?;
                    Command::new("pkexec")
                        .arg("rm")
                        .arg("-rf")
                        .arg("/tmp/icicle")
                        .output()?;
                    Ok(())
                }
                if let Err(e) = clear() {
                    error!("Failed to clear /tmp/icicle: {}", e);
                    let _ = sender.output(AppMsg::Error);
                    return;
                }

                // Step 1: Setup and mount partitions
                info!("Step 1: Setup and mount partitions");
                if let Err(e) = partition(*partitions.clone()) {
                    error!("Failed to partition: {}", e);
                    let _ = sender.output(AppMsg::Error);
                    return;
                }

                // Step 2: Generate base config
                info!("Step 2: Generate base config");
                if let Err(e) = Command::new("pkexec")
                    .arg("nixos-generate-config")
                    .arg("--root")
                    .arg("/tmp/icicle")
                    .output()
                {
                    error!("Failed to generate base config: {}", e);
                    let _ = sender.output(AppMsg::Error);
                    return;
                }

                if configtype == ConfigType::Snowfall {
                    // Move /tmp/icicle/etc/nixos/hardware-configuration.nix to /tmp/icicle/etc/nixos/systems/{ARCH}-linux/{HOSTNAME}/hardware.nix
                    Command::new("pkexec")
                        .arg("mkdir")
                        .arg("-p")
                        .arg(format!(
                            "/tmp/icicle/etc/nixos/systems/{}-linux/{}",
                            arch, hostname
                        ))
                        .output()
                        .unwrap();
                    Command::new("pkexec")
                        .arg("mv")
                        .arg("/tmp/icicle/etc/nixos/hardware-configuration.nix")
                        .arg(format!(
                            "/tmp/icicle/etc/nixos/systems/{}-linux/{}/hardware.nix",
                            arch, hostname
                        ))
                        .output()
                        .unwrap();
                    // Remove /tmp/icicle/etc/nixos/configuration.nix
                    Command::new("pkexec")
                        .arg("rm")
                        .arg("/tmp/icicle/etc/nixos/configuration.nix")
                        .output()
                        .unwrap();
                }

                // Step 3: Make configuration base on language, timezone, keyboard, and user
                info!("Step 3: Make configuration");

                let mut mbrdisk = None;
                if let Some(partitions) = partitions.as_ref() {
                    match partitions {
                        PartitionSchema::FullDisk(disk) => {
                            mbrdisk = Some(disk.to_string());
                        }
                        PartitionSchema::Custom(partitions) => {
                            for part in partitions.values() {
                                if part.mountpoint == Some("/".to_string()) {
                                    mbrdisk = Some(part.device.to_string());
                                }
                            }
                        }
                    }
                }

                if let Err(e) = makeconfig(MakeConfig {
                    id,
                    language,
                    timezone,
                    keyboard,
                    user: *user.clone(),
                    list: listconfig,
                    bootdisk: mbrdisk,
                }) {
                    error!("Failed to make config: {}", e);
                    let _ = sender.output(AppMsg::Error);
                    return;
                }

                // Step 4: Install NixOS
                info!("Step 4: Install NixOS");
                if let Some(hostname) = user.as_ref().as_ref().map(|u| u.hostname.clone()) {
                    INSTALL_BROKER.send(InstallMsg::Install(
                        vec![
                            "/usr/bin/env",
                            "pkexec",
                            "nixos-install",
                            "--root",
                            "/tmp/icicle",
                            "--no-root-passwd",
                            "--no-channel-copy",
                            "--flake",
                            &format!("/tmp/icicle/etc/nixos#{}", hostname),
                        ]
                        .into_iter()
                        .map(|s| s.to_string())
                        .collect(),
                    ));
                } else {
                    error!("No hostname found");
                    let _ = sender.output(AppMsg::Error);
                }
            }
            InstallAsyncMsg::FinishInstall => {
                // Step 5: Set user passwords
                info!("Step 5: Set user passwords");
                fn setuserpasswd(username: Option<String>, password: Option<String>) -> Result<()> {
                    let mut passwdcmd = Command::new("pkexec")
                        .arg("nixos-enter")
                        .arg("--root")
                        .arg("/tmp/icicle")
                        .arg("-c")
                        .arg("chpasswd")
                        .stdin(Stdio::piped())
                        .spawn()?;
                    let passwdstdin = passwdcmd
                        .stdin
                        .as_mut()
                        .context("Failed to get password stdin")?;
                    passwdstdin.write_all(
                        format!(
                            "{}:{}",
                            username.context("No username found")?,
                            password.context("No password found")?
                        )
                        .as_bytes(),
                    )?;
                    Ok(())
                }
                if let Err(e) = setuserpasswd(self.username.clone(), self.password.clone()) {
                    error!("Failed to set user password: {}", e);
                    let _ = sender.output(AppMsg::Error);
                    return;
                }

                // Step 6: Set root password
                info!("Step 6: Set root password if specified");
                if let Some(rootpasswd) = &self.rootpassword {
                    fn setrootpasswd(rootpasswd: String) -> Result<()> {
                        let mut rootpasswdcmd = Command::new("pkexec")
                            .arg("nixos-enter")
                            .arg("--root")
                            .arg("/tmp/icicle")
                            .arg("-c")
                            .arg("chpasswd")
                            .stdin(Stdio::piped())
                            .spawn()?;
                        let rootpasswdstdin = rootpasswdcmd
                            .stdin
                            .as_mut()
                            .context("Failed to get root password stdin")?;
                        rootpasswdstdin.write_all(format!("root:{}", rootpasswd).as_bytes())?;
                        Ok(())
                    }

                    if let Err(e) = setrootpasswd(rootpasswd.clone()) {
                        error!("Failed to set root password: {}", e);
                        let _ = sender.output(AppMsg::Error);
                        return;
                    }
                }

                let _ = sender.output(AppMsg::Finished);
            }
        }
    }
}

fn partition(partitions: Option<PartitionSchema>) -> Result<()> {
    let partitions = partitions.context("No partitions specified")?;
    let partjson = serde_json::to_string(&partitions)?;
    debug!("Executing partition with json: {}", partjson);
    let mut out = Command::new("pkexec")
        .arg(&format!("{}/icicle-helper", LIBEXECDIR))
        .arg("partition")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    out.stdin
        .take()
        .context("Failed to write to stdin")?
        .write_all(partjson.as_bytes())?;
    let mut stdout = BufReader::new(out.stdout.as_mut().context("Failed to get stdout")?);
    let mut line = String::new();
    while stdout.read_line(&mut line)? > 0 {
        debug!("PARTITION OUTPUT: {}", line.trim());
        line.clear();
    }
    let output = out
        .wait_with_output()
        .context("Failed to wait for output")?;
    if output.status.success() {
        Ok(())
    } else {
        error!(
            "Partitioning failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Err(anyhow!(
            "Partitioning failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

pub struct MakeConfig {
    pub id: String,
    pub language: Option<String>,
    pub timezone: Option<String>,
    pub keyboard: Option<String>,
    pub user: Option<UserConfig>,
    pub list: HashMap<String, HashMap<String, Choice>>,
    pub bootdisk: Option<String>,
}

pub fn makeconfig(makeconfig: MakeConfig) -> Result<()> {
    /* Configuration keys:
        @NVIDIAOFFLOAD@ - Enable NVIDIA offloading
        @BOOTLOADRER@ - Bootloader
        @NETWORK@ - Network configuration
        @TIMEZONE@ - Timezone
        @LOCALE@ - Localization
        @KEYBOARD@ - Keyboard layout
        @DESKTOP@ - Desktop environment
        @AUTOLOGIN@ - Autologin config
        @PACKAGES@ - Packages to install
        @STATEVERSION@ - NixOS State version
    */

    /* Value keys:
        @HOSTNAME@ - Hostname
        @USERNAME@ - Username
        @FULLNAME@ - Full name
    */

    let efi = distinst_disks::Bootloader::detect() == distinst_disks::Bootloader::Efi;
    let archout = Command::new("uname")
        .arg("-m")
        .output()
        .context("Failed to get architecture")?;
    let arch = String::from_utf8_lossy(&archout.stdout).trim().to_string();

    fn iterwrite(makeconfig: &MakeConfig, path: &str, efi: bool, arch: &str) -> Result<()> {
        // Iterate through files in configs/
        for file in
            (fs::read_dir(&format!("{}/icicle/{}/{}", SYSCONFDIR, makeconfig.id, path))?).flatten()
        {
            // Check if it is a dir
            if file.metadata()?.is_dir() {
                // Iterate through files in the dir
                let _ = iterwrite(
                    makeconfig,
                    &format!("{}/{}", path, file.file_name().to_string_lossy()),
                    efi,
                    arch,
                );
            } else if file.file_name().to_string_lossy().ends_with(".nix") {
                let mut config = fs::read_to_string(file.path())?;
                config = config.replace("@NVIDIAOFFLOAD@", "");

                config = config.replace("@ARCH@", &format!("{}-linux", arch));

                if efi {
                    config = config.replace(
                        "@BOOTLOADER@",
                        r#"  # Bootloader.
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;
  boot.loader.efi.efiSysMountPoint = "/boot/efi";"#,
                    );
                } else {
                    config = config.replace(
                        "@BOOTLOADER@",
                        &format!(
                            r#"  # Bootloader.
  boot.loader.grub.enable = true;
  boot.loader.grub.device = "{}";
  boot.loader.grub.useOSProber = true;"#,
                            makeconfig
                                .bootdisk
                                .as_ref()
                                .context("Failed to get bootloader disk")?
                        ),
                    );
                }

                config = config.replace(
                    "@NETWORK@",
                    &format!(
                        r#"  # Define your hostname.
  networking.hostName = "{}";

  # Enable networking
  networking.networkmanager.enable = true;"#,
                        makeconfig
                            .user
                            .as_ref()
                            .map(|x| x.hostname.as_ref())
                            .unwrap_or("nixos")
                    ),
                );

                if let Some(tz) = &makeconfig.timezone {
                    config = config.replace(
                        "@TIMEZONE@",
                        &format!(
                            r#"  # Set your time zone.
  time.timeZone = "{}";"#,
                            tz
                        ),
                    );
                }

                if let Some(locale) = &makeconfig.language {
                    config = config.replace(
                        "@LOCALE@",
                        &format!(
                            r#"  # Select internationalisation properties.
  i18n.defaultLocale = "{}";"#,
                            locale
                        ),
                    );
                }

                if let Some(keymap) = &makeconfig.keyboard {
                    if keymap.contains('+') {
                        let mut split = keymap.split('+');
                        if let (Some(layout), Some(variant)) = (split.next(), split.next()) {
                            config = config.replace(
                                "@KEYBOARD@",
                                &format!(
                                    r#"  # Set the keyboard layout.
  services.xserver = {{
    layout = "{}";
    xkbVariant = "{}";
  }};
  console.useXkbConfig = true;"#,
                                    layout, variant
                                ),
                            );
                        }
                    } else {
                        config = config.replace(
                            "@KEYBOARD@",
                            &format!(
                                r#"  # Set the keyboard layout.
  services.xserver.layout = "{}";
  console.useXkbConfig = true;"#,
                                keymap
                            ),
                        );
                    }
                }

                config = config.replace(
                    "@DESKTOP@",
                    r#"  # Enable the X11 windowing system.
  services.xserver.enable = true;
  # Enable the GNOME Desktop Environment.
  services.xserver.displayManager.gdm.enable = true;
  services.xserver.desktopManager.gnome.enable = true;"#,
                );

                if let Some(user) = &makeconfig.user {
                    config = config.replace("@USERNAME@", &user.username);
                    config = config.replace("@FULLNAME@", &user.name);
                    config = config.replace("@HOSTNAME@", &user.hostname);

                    let mut autocfg = String::new();
                    if user.autologin {
                        autocfg.push_str(&format!(
                            r#"  # Enable automatic login for the user.
  services.xserver.displayManager.autoLogin.enable = true;
  services.xserver.displayManager.autoLogin.user = "{}";
"#,
                            user.username
                        ));
                        autocfg.push_str(
                                    r#"  # Workaround for GNOME autologin: https://github.com/NixOS/nixpkgs/issues/103746#issuecomment-945091229
  systemd.services."getty@tty1".enable = false;
  systemd.services."autovt@tty1".enable = false;
"#,
                                );
                    }
                    config = config.replace("@AUTOLOGIN@", &autocfg);
                }

                // List configuration options
                let mut extrapkgs = vec![];
                for (id, choices) in makeconfig.list.iter() {
                    let mut listcfg = String::new();
                    for (_key, choice) in choices.iter() {
                        if let Some(pkgs) = &choice.packages {
                            for pkg in pkgs {
                                extrapkgs.push(pkg.to_string());
                            }
                        }
                        if let Some(cfg) = &choice.config {
                            cfg.lines()
                                .for_each(|x| listcfg.push_str(&format!("  {}\n", x)));
                        }
                    }
                    config = config.replace(&format!("@{}@", id), &listcfg);
                }

                config = config.replace(
                    "@PACKAGES@",
                    &if extrapkgs.is_empty() {
                        r#"  # List packages installed in system profile.
  environment.systemPackages = with pkgs; [
    firefox
  ];"#
                        .to_string()
                    } else {
                        format!(
                            r#"  # List packages installed in system profile.
  environment.systemPackages = with pkgs; [
    firefox
    {}
  ];"#,
                            extrapkgs.join("\n    ")
                        )
                    },
                );

                config = config.replace(
                    "@STATEVERSION@",
                    &format!(
                        r#"  system.stateVersion = "{}"; # Did you read the comment?"#,
                        String::from_utf8_lossy(
                            &Command::new("nixos-version")
                                .output()
                                .context("Failed to get nixos version")?
                                .stdout
                        )
                        .to_string()
                        .get(0..5)
                        .context("Failed to get nixos version")?
                    ),
                );

                let mut cmd = Command::new("pkexec")
                    .arg(&format!("{}/icicle-helper", LIBEXECDIR))
                    .arg("write-file")
                    .arg("--path")
                    .arg(if path.is_empty() {
                        format!(
                            "/tmp/icicle/etc/nixos/{}",
                            file.file_name().to_string_lossy()
                        )
                    } else {
                        format!(
                            "/tmp/icicle/etc/nixos/{}/{}",
                            path.replace("ARCH", &format!("{}-linux", arch)).replace(
                                "HOSTNAME",
                                makeconfig
                                    .user
                                    .as_ref()
                                    .map(|x| x.hostname.as_ref())
                                    .unwrap_or("nixos")
                            ),
                            file.file_name().to_string_lossy()
                        )
                    })
                    .arg("--contents")
                    .arg(config)
                    .spawn()?;
                cmd.wait()?;
            }
        }
        Ok(())
    }

    iterwrite(&makeconfig, "", efi, &arch)
}
