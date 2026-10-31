import os
from pathlib import Path
import subprocess
import unittest


@unittest.skipUnless(os.name == "nt", "Windows routing script")
class RouteScriptTests(unittest.TestCase):
    def test_transforms_preserve_foreign_entries_and_refuse_conflicts(self):
        script = Path(__file__).resolve().parents[1] / "rl_uploader" / "proxy_route.ps1"
        quoted = "'" + str(script).replace("'", "''") + "'"
        checks = r'''
$ErrorActionPreference = 'Stop'
$fixture = "127.0.0.1 localhost`r`n10.0.0.1 unrelated.example # keep this`r`n"
foreach ($ipv6 in @($true, $false)) {
    $added = Get-RluRouteText $fixture $true $ipv6
    if ((Get-RluRouteText $added $false $false) -cne $fixture) { throw 'Foreign content changed' }
    if ((Get-RluRouteText $added $true $ipv6) -cne $added) { throw 'Non-idempotent route' }
}
$rejected = $false
try { $null = Get-RluRouteText "1.2.3.4 alias ws.rlpp.psynet.gg`r`n" $true $false } catch { $rejected = $true }
if (-not $rejected) { throw 'Existing mapping was not rejected' }
$rejected = $false
try { $null = Get-RluRouteText "# BEGIN RL UPLOADER WEBSOCKET`r`nforeign data`r`n" $false $false } catch { $rejected = $true }
if (-not $rejected) { throw 'Malformed block was not rejected' }
Write-Output 'Pure transformations passed'
'''
        result = subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                                 ". " + quoted + " -ValidateOnly; " + checks], capture_output=True, text=True,
                                timeout=15, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Pure transformations passed", result.stdout)


if __name__ == "__main__":
    unittest.main()
