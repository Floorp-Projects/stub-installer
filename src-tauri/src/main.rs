#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use reqwest::Client;
use std::env;
use std::path::PathBuf;
use std::{
    ffi::{c_void, OsStr},
    mem::transmute,
    os::windows::ffi::OsStrExt,
    ptr::null_mut,
    fs,
    io::{self, Read, Write},
};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::{sleep, Duration};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::Foundation::{GetLastError, HANDLE, HWND},
    Win32::Security::Cryptography::{
        CertGetNameStringW, CryptQueryObject, CERT_NAME_SIMPLE_DISPLAY_TYPE,
        CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED, CERT_QUERY_CONTENT_TYPE,
        CERT_QUERY_ENCODING_TYPE, CERT_QUERY_FORMAT_FLAG_BINARY, CERT_QUERY_FORMAT_TYPE,
        CERT_QUERY_OBJECT_FILE, HCERTSTORE,
    },
    Win32::Security::WinTrust::{
        WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0,
        WINTRUST_DATA_PROVIDER_FLAGS, WINTRUST_DATA_UICONTEXT, WINTRUST_FILE_INFO, WTD_CHOICE_FILE,
        WTD_REVOKE_NONE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
    },
};

const EXPECTED_SIGNERS: [&str; 2] = [
    "SignPath Foundation",
    "Open Source Developer, Ryosuke Asano",
];

#[tauri::command]
async fn download_and_run_installer(
    use_admin: bool,
    custom_install_path: Option<String>,
) -> Result<String, String> {
    let url = match get_latest_installer_url().await {
        Some(url) => url,
        None => return Err("rust.errors.installer_not_found".to_string()),
    };

    println!(
        "[INFO] Downloading Floorp installer from: {}",
        url
    );
    println!(
        "[INFO] Installation mode: {}",
        if use_admin {
            "Administrator privileges"
        } else {
            "User privileges"
        }
    );

    if let Some(path) = &custom_install_path {
        println!("[INFO] Custom installation path: {}", path);
    }

    let filename = url.split('/').last().unwrap();
    let temp_dir = env::temp_dir();
    let path = temp_dir.join(filename);

    if let Err(e) = download_file(&url, &path).await {
        return Err(format!("rust.errors.download_failed|{}", e));
    }

    {
        match check_downloaded_installer_code_sign(&path).await {
            Ok(true) => println!("[INFO] Installer signature verification successful"),
            Ok(false) => {
                return Err("rust.errors.signature_verification_failed".to_string())
            }
            Err(e) => return Err(format!("rust.errors.signature_verification_error|{}", e)),
        }
    }

    if env::var("RUST_ENV")
        .map(|v| v == "development")
        .unwrap_or(false)
    {
        println!("[INFO] Development mode: Skipping installer execution");
        return Ok("rust.success.downloaded_dev_mode".to_string());
    }

    println!("[INFO] Running Floorp installer...");
    match run_installer(&path, use_admin, custom_install_path).await {
        Ok(status) => {
            if status.success {
                return Ok("rust.success.installation_complete".to_string());
            } else {
                return Err(format!("rust.errors.installer_exit_code|{}", status.code));
            }
        }
        Err(e) => {
            return Err(format!("rust.errors.installer_execution|{}", e))
        }
    }
}

#[tauri::command]
async fn launch_floorp_browser() -> Result<(), String> {
    println!("[INFO] Launching Floorp browser");

    let mut possible_paths = Vec::new();

    if let Ok(custom_path) = get_saved_install_path() {
        println!("[INFO] Checking saved installation path: {}", custom_path);
        let custom_exe_path = PathBuf::from(format!("{}\\floorp.exe", custom_path));
        if custom_exe_path.exists() {
            println!("[INFO] Found Floorp at custom installation path: {}", custom_exe_path.display());
            return launch_browser(&custom_exe_path);
        } else {
            println!("[WARN] Floorp not found at custom installation path: {}", custom_exe_path.display());
        }
    }

    if let Ok(local_appdata) = env::var("LOCALAPPDATA") {
        let user_path = PathBuf::from(format!("{}\\Ablaze Floorp\\floorp.exe", local_appdata));
        possible_paths.push(user_path);
    }

    if let Ok(program_files) = env::var("ProgramFiles") {
        let admin_path = PathBuf::from(format!("{}\\Ablaze Floorp\\floorp.exe", program_files));
        possible_paths.push(admin_path);
    }

    possible_paths.push(PathBuf::from(
        "C:\\Program Files\\Ablaze Floorp\\floorp.exe",
    ));
    possible_paths.push(PathBuf::from(
        "C:\\Program Files (x86)\\Ablaze Floorp\\floorp.exe",
    ));

    let mut floorp_path = None;
    for path in possible_paths {
        if path.exists() {
            println!(
                "[INFO] Found Floorp browser at: {}",
                path.display()
            );
            floorp_path = Some(path);
            break;
        }
    }

    let floorp_path = match floorp_path {
        Some(path) => path,
        None => return Err("rust.errors.browser_not_found".to_string())
    };

    launch_browser(&floorp_path)
}

fn launch_browser(path: &PathBuf) -> Result<(), String> {
    match Command::new(path).spawn() {
        Ok(_) => {
            println!("[INFO] Successfully launched Floorp browser");
            Ok(())
        }
        Err(e) => {
            println!("[ERROR] Failed to launch Floorp browser: {}", e);
            Err(format!("rust.errors.browser_launch_failed|{}", e))
        }
    }
}

fn get_install_path_file() -> PathBuf {
    let app_data_dir = if let Ok(local_appdata) = env::var("LOCALAPPDATA") {
        PathBuf::from(local_appdata)
    } else {
        env::temp_dir()
    };
    app_data_dir.join("Floorp-Installer").join("install_path.txt")
}

fn save_install_path(install_path: &str) -> io::Result<()> {
    let path_file = get_install_path_file();

    if let Some(parent) = path_file.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = fs::File::create(path_file)?;
    file.write_all(install_path.as_bytes())?;
    println!("[INFO] Saved installation path: {}", install_path);
    Ok(())
}

fn get_saved_install_path() -> io::Result<String> {
    let path_file = get_install_path_file();
    if !path_file.exists() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "Saved installation path not found"));
    }

    let mut file = fs::File::open(path_file)?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content.trim().to_string())
}

#[tauri::command]
async fn exit_application() -> Result<(), String> {
    println!("[INFO] Exiting application");

    sleep(Duration::from_millis(500)).await;

    std::process::exit(0);
}

async fn get_latest_installer_url() -> Option<String> {
    let client = Client::new();
    let resp = client
        .get("https://api.github.com/repos/Floorp-Projects/Floorp/releases/latest")
        .header("User-Agent", "Floorp-Installer")
        .send()
        .await
        .ok()?;

    let json: serde_json::Value = resp.json().await.ok()?;

    for asset in json["assets"].as_array()? {
        let name = asset["name"].as_str()?;
        if name.ends_with(".exe") {
            return asset["browser_download_url"]
                .as_str()
                .map(|s| s.to_string());
        }
    }
    None
}

async fn download_file(url: &str, path: &PathBuf) -> Result<(), String> {
    let client = Client::new();
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("HTTP request error: {}", e))?
        .bytes()
        .await
        .map_err(|e| format!("Response read error: {}", e))?;

    let mut file = File::create(path)
        .await
        .map_err(|e| format!("File creation error: {}", e))?;

    file.write_all(&resp)
        .await
        .map_err(|e| format!("File write error: {}", e))?;

    Ok(())
}

fn wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn verify_signature(path: &PathBuf) -> Result<bool, String> {
    let path_str = path.to_string_lossy().to_string();
    let file_path = wide_null(&path_str);

    let signature_valid = verify_signature_validity(&file_path)?;
    if !signature_valid {
        return Ok(false);
    }

    match get_signer_name(&file_path)? {
        Some(signer_name) => {
            println!("[INFO] Signer: {}", signer_name);

            let is_trusted = EXPECTED_SIGNERS
                .iter()
                .any(|&trusted| signer_name.contains(trusted));

            if is_trusted {
                println!("[INFO] Signer is in the trusted list");
                Ok(true)
            } else {
                println!(
                    "[WARN] Signer is not in the trusted list: {}",
                    signer_name
                );
                Ok(false)
            }
        }
        None => {
            println!("[WARN] Could not retrieve signer information");
            Ok(false)
        }
    }
}

fn verify_signature_validity(file_path: &[u16]) -> Result<bool, String> {
    let file_info_box = Box::new(WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(file_path.as_ptr()),
        hFile: HANDLE::default(),
        pgKnownSubject: null_mut(),
    });

    let file_info_ptr = Box::into_raw(file_info_box);

    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        pPolicyCallbackData: null_mut(),
        pSIPClientData: null_mut(),
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: file_info_ptr,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        hWVTStateData: HANDLE::default(),
        pwszURLReference: PWSTR::null(),
        dwProvFlags: WINTRUST_DATA_PROVIDER_FLAGS(0),
        dwUIContext: WINTRUST_DATA_UICONTEXT(0),
        pSignatureSettings: null_mut(),
    };

    let action_id = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let result = unsafe {
        let ret = WinVerifyTrust(
            HWND::default(),
            &action_id as *const _ as *mut _,
            &mut data as *mut _ as *mut _,
        );

        let _ = Box::from_raw(file_info_ptr);

        ret
    };

    println!("[INFO] Windows API signature verification result: {}", result);

    Ok(result == 0)
}

fn get_signer_name(file_path: &[u16]) -> Result<Option<String>, String> {
    unsafe {
        let mut encoding_type = CERT_QUERY_ENCODING_TYPE::default();
        let mut content_type = CERT_QUERY_CONTENT_TYPE::default();
        let mut format_type = CERT_QUERY_FORMAT_TYPE::default();
        let mut cert_store: HCERTSTORE = HCERTSTORE::default();
        let mut msg = null_mut::<c_void>();
        let mut context = null_mut::<c_void>();

        let success = CryptQueryObject(
            CERT_QUERY_OBJECT_FILE,
            transmute(PCWSTR(file_path.as_ptr())),
            CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
            CERT_QUERY_FORMAT_FLAG_BINARY,
            0,
            Some(&mut encoding_type),
            Some(&mut content_type),
            Some(&mut format_type),
            Some(&mut cert_store),
            Some(&mut msg),
            Some(&mut context),
        );

        if !success.is_ok() {
            let error = GetLastError();
            return Err(format!(
                "CryptQueryObject failed: error code {}",
                error.0
            ));
        }

        let cert_context = windows::Win32::Security::Cryptography::CertFindCertificateInStore(
            cert_store,
            encoding_type,
            0,
            windows::Win32::Security::Cryptography::CERT_FIND_ANY,
            Some(null_mut::<c_void>()),
            None,
        );

        if cert_context.is_null() {
            return Ok(None);
        }

        let mut name_buf = vec![0u16; 256];

        let name_len = CertGetNameStringW(
            cert_context,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            Some(null_mut::<c_void>()),
            Some(&mut name_buf),
        );

        if name_len == 0 {
            let _ = windows::Win32::Security::Cryptography::CertFreeCertificateContext(Some(
                cert_context,
            ));
            return Ok(None);
        }

        let name = String::from_utf16_lossy(&name_buf[0..(name_len as usize - 1)]);

        let _ =
            windows::Win32::Security::Cryptography::CertFreeCertificateContext(Some(cert_context));

        Ok(Some(name))
    }
}

async fn check_downloaded_installer_code_sign(path: &PathBuf) -> Result<bool, String> {
    if !path.exists() {
        return Err(format!("rust.errors.file_not_found|{}", path.display()));
    }
    if !path.is_file() {
        return Err(format!("rust.errors.not_a_file|{}", path.display()));
    }

    return verify_signature(path);
}

struct InstallerStatus {
    success: bool,
    code: i32,
}

async fn run_installer(
    path: &PathBuf,
    use_admin: bool,
    custom_install_path: Option<String>,
) -> Result<InstallerStatus, String> {
    println!("[INFO] Running installer: {}", path.display());

    let temp_dir = env::temp_dir();
    let config_ini = temp_dir.join("floorp_install_config.ini");

    let install_dir = if let Some(custom_path) = &custom_install_path {
        custom_path.clone()
    } else if use_admin {
        if let Ok(program_files) = env::var("ProgramFiles") {
            format!("{}\\Ablaze Floorp", program_files)
        } else {
            "C:\\Program Files\\Ablaze Floorp".to_string()
        }
    } else {
        if let Ok(local_appdata) = env::var("LOCALAPPDATA") {
            format!("{}\\Ablaze Floorp", local_appdata)
        } else {
            "C:\\Program Files\\Ablaze Floorp".to_string()
        }
    };

    println!("[INFO] Installation directory: {}", install_dir);

    save_install_path(&install_dir).map_err(|e| format!("rust.errors.save_install_path|{}", e))?;

    let config_content = format!(
        "[Install]\r\nInstallDirectoryPath={}\r\nTaskbarShortcut=true\r\nDesktopShortcut=true\r\nStartMenuShortcuts=true\r\nMaintenanceService=false\r\n",
        install_dir
    );

    match std::fs::write(&config_ini, config_content) {
        Ok(_) => println!(
            "[INFO] Created installation config file: {}",
            config_ini.display()
        ),
        Err(e) => println!("[WARN] Failed to create installation config file: {}", e),
    }

    let mut installer_args = vec!["/S".to_string()];

    if !use_admin {
        installer_args.push("/CURRENTUSER".to_string());
    }

    if config_ini.exists() {
        installer_args.push(format!("/INI={}", config_ini.to_string_lossy()));
    }

    if let Some(custom_path) = &custom_install_path {
        installer_args.push(format!("/InstallDirectoryPath={}", custom_path));
    }

    let installer_args_str = installer_args.join(" ");
    let status;

    if use_admin {
        println!("[INFO] Running installer with administrator privileges");

        let script_path = temp_dir.join("run_floorp_installer.ps1");
        let script_content = format!(
            "try {{\n\
             $ErrorActionPreference = 'Stop';\n\
             $process = Start-Process -FilePath '{}' -ArgumentList '{}' -Verb RunAs -PassThru -Wait;\n\
             $exitCode = $process.ExitCode;\n\
             Write-Output \"EXITCODE:$exitCode\";\n\
             exit 0;\n\
             }} catch {{\n\
             Write-Output \"ERROR:$($_.Exception.Message)\";\n\
             exit 1;\n\
             }}",
            path.to_string_lossy().replace("'", "''"),  // Escape single quotes for PowerShell
            installer_args_str.replace("'", "''")
        );

        match std::fs::write(&script_path, script_content) {
            Ok(_) => println!(
                "[INFO] Created PowerShell script: {}",
                script_path.display()
            ),
            Err(e) => return Err(format!("rust.errors.powershell_script_creation|{}", e)),
        }

        let child = Command::new("powershell.exe")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&script_path)
            .output()
            .await
            .map_err(|e| format!("rust.errors.powershell_process|{}", e))?;

        let _ = std::fs::remove_file(&script_path);

        let output = String::from_utf8_lossy(&child.stdout);
        let error_output = String::from_utf8_lossy(&child.stderr);

        println!("[DEBUG] PowerShell stdout: {}", output);

        if !error_output.is_empty() {
            println!("[DEBUG] PowerShell stderr: {}", error_output);
        }

        if output.contains("ERROR:")
            && (output.contains("RunAs")
                || output.contains("permission")
                || output.contains("canceled")
                || output.contains("cancelled")
                || output.contains("denied"))
        {
            return Err("rust.errors.admin_rights_denied".to_string());
        }

        let exit_code = if let Some(code_line) =
            output.lines().find(|line| line.starts_with("EXITCODE:"))
        {
            code_line
                .trim_start_matches("EXITCODE:")
                .trim()
                .parse::<i32>()
                .unwrap_or(-1)
        } else {
            return Err("rust.errors.exit_code_not_found".to_string());
        };

        println!(
            "[INFO] Admin installation completed. Exit code: {}",
            exit_code
        );

        if exit_code == 0 {
            status = InstallerStatus {
                success: true,
                code: 0,
            };
        } else {
            return Err(format!("rust.errors.admin_install_failed|{}", exit_code));
        }
    } else {
        status = run_installer_user_mode(path, &config_ini, custom_install_path.as_deref()).await?;
    }

    if config_ini.exists() {
        let _ = std::fs::remove_file(&config_ini);
    }

    sleep(Duration::from_secs(2)).await;

    Ok(status)
}

async fn run_installer_user_mode(
    path: &PathBuf,
    config_ini: &PathBuf,
    custom_install_path: Option<&str>,
) -> Result<InstallerStatus, String> {
    println!("[INFO] Running installer in user mode");

    let user_config_ini = if !config_ini.exists() {
        let temp_dir = env::temp_dir();
        let user_config = temp_dir.join("floorp_user_install_config.ini");

        let user_install_dir = if let Some(custom_path) = custom_install_path {
            custom_path.to_string()
        } else if let Ok(local_appdata) = env::var("LOCALAPPDATA") {
            format!("{}\\Ablaze Floorp", local_appdata)
        } else {
            "C:\\Program Files\\Ablaze Floorp".to_string()
        };

        let config_content = format!(
            "[Install]\r\nInstallDirectoryPath={}\r\nTaskbarShortcut=true\r\nDesktopShortcut=true\r\nStartMenuShortcuts=true\r\nMaintenanceService=false\r\n",
            user_install_dir
        );

        match std::fs::write(&user_config, config_content) {
            Ok(_) => {
                println!(
                    "[INFO] Created user mode installation config file: {}",
                    user_config.display()
                );
                Some(user_config)
            }
            Err(e) => {
                println!("[WARN] Failed to create installation config file: {}", e);
                None
            }
        }
    } else {
        None
    };

    let mut args = vec!["/S".to_string(), "/CURRENTUSER".to_string()];

    if config_ini.exists() {
        args.push(format!("/INI={}", config_ini.to_string_lossy()));
    } else if let Some(ref user_config) = user_config_ini {
        args.push(format!("/INI={}", user_config.to_string_lossy()));
    }

    if let Some(custom_path) = custom_install_path {
        args.push(format!("/InstallDirectoryPath={}", custom_path));
    }

    println!(
        "[INFO] User mode installer arguments: {}",
        args.join(" ")
    );

    let mut child = Command::new(path).args(&args).spawn().map_err(|e| {
        format!("rust.errors.user_installer_launch|{}", e)
    })?;

    let result = match child.wait().await {
        Ok(status) => {
            let exit_code = status.code().unwrap_or(-1);
            println!(
                "[INFO] User mode installation completed. Exit code: {}",
                exit_code
            );

            Ok(InstallerStatus {
                success: exit_code == 0,
                code: exit_code,
            })
        }
        Err(e) => Err(format!("rust.errors.user_installer_execution|{}", e)),
    };

    if let Some(user_config) = user_config_ini {
        let _ = std::fs::remove_file(user_config);
    }

    result
}

fn main() {
    #[cfg(debug_assertions)]
    // env::set_var("RUST_ENV", "development");
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            download_and_run_installer,
            launch_floorp_browser,
            exit_application
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
