param(
    [Parameter(Mandatory = $true)]
    [string]$OutputPath,
    [int]$Left = 0,
    [int]$Top = 1420,
    [int]$Width = 2560,
    [int]$Height = 180
)

# 只读验收工具：按 Per-Monitor DPI 坐标截取副屏底部任务栏区域，不操控任何窗口。
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type -ReferencedAssemblies @(
    'C:\Windows\Microsoft.NET\Framework64\v4.0.30319\System.Drawing.dll',
    'C:\Windows\Microsoft.NET\Framework64\v4.0.30319\System.Windows.Forms.dll'
) @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Windows.Forms;
using System.Runtime.InteropServices;
public static class TaskbarCapture {
    [DllImport("user32.dll")]
    public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr value);

    public static void Save(string path, int left, int top, int width, int height) {
        SetThreadDpiAwarenessContext(new IntPtr(-4)); // PER_MONITOR_AWARE_V2
        using (var bitmap = new Bitmap(width, height))
        using (var graphics = Graphics.FromImage(bitmap)) {
            graphics.CopyFromScreen(left, top, 0, 0, bitmap.Size, CopyPixelOperation.SourceCopy);
            bitmap.Save(path, ImageFormat.Png);
        }
    }
}
'@

[TaskbarCapture]::Save($OutputPath, $Left, $Top, $Width, $Height)
