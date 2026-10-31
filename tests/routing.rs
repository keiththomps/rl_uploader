#[cfg(windows)]
#[test]
fn embedded_watchdog_preserves_foreign_entries_and_removes_only_owned_block() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use rl_uploader::{platform, proxy::ROUTE_SCRIPT};
    // ValidateOnly returns before any admin check, hosts edit, or scheduled task operation.
    let checks = r##"
$original = "# arbitrary comment`r`n127.0.0.1 localhost`r`n192.0.2.1 other.example`r`n"
$enabled = Get-RluRouteText $original $true $true
if ($enabled -notlike '*::1 ws.rlpp.psynet.gg*') { throw 'IPv6 mapping missing' }
if ((Get-RluRouteText $enabled $true $true) -cne $enabled) { throw 'Not idempotent' }
if ((Get-RluRouteText $enabled $false $false) -cne $original) { throw 'Foreign lines changed' }
$rejected=$false
try { Get-RluRouteText '192.0.2.2 ws.rlpp.psynet.gg' $true $false | Out-Null } catch { $rejected=$true }
if (-not $rejected) { throw 'Accepted a foreign mapping' }
$rejected=$false
try { Get-RluRouteText '# BEGIN RL UPLOADER WEBSOCKET' $false $false | Out-Null } catch { $rejected=$true }
if (-not $rejected) { throw 'Accepted a malformed block' }
Write-Output 'ROUTE_TRANSFORMS_OK'
"##;
    let script = format!(". {{\n{ROUTE_SCRIPT}\n}} -ValidateOnly\n{checks}");
    let encoded = STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    let mut cmd = platform::hidden_command("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded]);
    let output = platform::command_output(cmd, std::time::Duration::from_secs(20)).unwrap();
    assert!(output.status.success(), "Pure route-transform tests failed");
    assert!(String::from_utf8_lossy(&output.stdout).contains("ROUTE_TRANSFORMS_OK"));
}
