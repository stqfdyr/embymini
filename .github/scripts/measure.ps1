# Helpers for the gpui workflow: full-screen screenshot and memory of a set of processes.

function Save-Screen([string]$Path) {
  try {
    Add-Type -AssemblyName System.Windows.Forms, System.Drawing
    $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
    [System.Drawing.Graphics]::FromImage($bmp).CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size)
    $bmp.Save((Join-Path $PWD $Path))
  } catch { Write-Host "截图失败（不影响判断）：$_" }
}

# Sum of working set and private bytes over every process with one of these names
function Get-Memory([string[]]$Names) {
  $ps = Get-Process -Name $Names -ErrorAction SilentlyContinue
  [ordered]@{
    processes      = @($ps).Count
    working_set_mb = [math]::Round((($ps | Measure-Object WorkingSet64 -Sum).Sum) / 1MB, 1)
    private_mb     = [math]::Round((($ps | Measure-Object PrivateMemorySize64 -Sum).Sum) / 1MB, 1)
  }
}
