use hbb_common::{
    anyhow::anyhow,
    base64::{engine::general_purpose::STANDARD, Engine},
    ResultType,
};
use std::{os::windows::process::CommandExt, process::Command};

const CREATE_NO_WINDOW: u32 = 0x08000000;

fn powershell(script: &str) -> ResultType<String> {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let result = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &STANDARD.encode(utf16),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    if !result.status.success() {
        return Err(anyhow!(
            "Install VB-CABLE and AudioDeviceCmdlets: {}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

pub struct Route {
    previous: String,
    previous_communication: String,
    target: String,
    restored: bool,
}

impl Route {
    pub fn open() -> ResultType<Self> {
        const SCRIPT: &str = "$ErrorActionPreference='Stop'; Import-Module AudioDeviceCmdlets; $prev=(Get-AudioDevice -Recording).ID; $prevComm=(Get-AudioDevice -RecordingCommunication).ID; if ([string]::IsNullOrEmpty($prev)) { throw 'No default recording device' }; if ([string]::IsNullOrEmpty($prevComm)) { throw 'No default communication recording device' }; $d=@(Get-AudioDevice -List | Where-Object { $_.Type -eq 'Recording' -and $_.Name -like 'CABLE Output*' }); if ($d.Count -ne 1) { throw 'Exactly one VB-CABLE recording endpoint is required' }; $t=$d[0].ID; if ([string]::IsNullOrEmpty($t)) { throw 'VB-CABLE recording endpoint has no ID' }; $payload=[ordered]@{previous=$prev; previousCommunication=$prevComm; target=$t} | ConvertTo-Json -Compress; try { Set-AudioDevice -ID $t | Out-Null } catch { if ((Get-AudioDevice -Recording).ID -eq $t) { Set-AudioDevice -ID $prev -DefaultOnly | Out-Null }; if ((Get-AudioDevice -RecordingCommunication).ID -eq $t) { Set-AudioDevice -ID $prevComm -CommunicationOnly | Out-Null }; throw }; if ((Get-AudioDevice -Recording).ID -ne $t -or (Get-AudioDevice -RecordingCommunication).ID -ne $t) { if ((Get-AudioDevice -Recording).ID -eq $t) { Set-AudioDevice -ID $prev -DefaultOnly | Out-Null }; if ((Get-AudioDevice -RecordingCommunication).ID -eq $t) { Set-AudioDevice -ID $prevComm -CommunicationOnly | Out-Null }; throw 'VB-CABLE capture switch was not applied' }; Write-Output $payload";
        let stdout = powershell(SCRIPT)?;
        let value: serde_json::Value = serde_json::from_str(&stdout)
            .map_err(|error| anyhow!("VB-CABLE switch response was not valid JSON: {error}: {stdout}"))?;
        let get = |key: &str| -> ResultType<String> {
            let text = value
                .get(key)
                .and_then(|item| item.as_str())
                .ok_or_else(|| anyhow!("VB-CABLE switch response missing '{key}': {stdout}"))?;
            if text.is_empty() {
                return Err(anyhow!("VB-CABLE switch response has empty '{key}': {stdout}"));
            }
            Ok(text.to_owned())
        };
        let previous = get("previous")?;
        let previous_communication = get("previousCommunication")?;
        let target = get("target")?;
        Ok(Self {
            previous,
            previous_communication,
            target,
            restored: false,
        })
    }

    pub fn output(&self) -> &str {
        "CABLE Input"
    }

    pub fn restore(&mut self) {
        if self.restored {
            return;
        }
        let script = format!("$ErrorActionPreference='Stop'; Import-Module AudioDeviceCmdlets; if ((Get-AudioDevice -Recording).ID -eq '{}') {{ Set-AudioDevice -ID '{}' -DefaultOnly | Out-Null }}; if ((Get-AudioDevice -RecordingCommunication).ID -eq '{}') {{ Set-AudioDevice -ID '{}' -CommunicationOnly | Out-Null }}", self.target.replace('\'', "''"), self.previous.replace('\'', "''"), self.target.replace('\'', "''"), self.previous_communication.replace('\'', "''"));
        if let Err(error) = powershell(&script) {
            hbb_common::log::warn!("{error}");
        }
        self.restored = true;
    }
}
