# Sends key presses into the console of a running process (for testing rmc's real main loop).
# Usage: sendkey.ps1 -ProcessId <pid> -Keys 'ctrl+o' | 'text:hello' | 'enter' ...
param([int] $ProcessId, [string[]] $Keys)

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class ConsoleKeys {
    [StructLayout(LayoutKind.Explicit, CharSet = CharSet.Unicode)]
    public struct INPUT_RECORD {
        [FieldOffset(0)] public ushort EventType;
        [FieldOffset(4)] public int KeyDown;
        [FieldOffset(8)] public ushort RepeatCount;
        [FieldOffset(10)] public ushort VirtualKeyCode;
        [FieldOffset(12)] public ushort VirtualScanCode;
        [FieldOffset(14)] public char UnicodeChar;
        [FieldOffset(16)] public uint ControlKeyState;
    }

    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool FreeConsole();
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool AttachConsole(uint pid);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    public static extern IntPtr CreateFileW(string name, uint access, uint share, IntPtr sa, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool WriteConsoleInputW(IntPtr h, INPUT_RECORD[] records, uint count, out uint written);

    static INPUT_RECORD Key(bool down, ushort vk, char ch, uint state) {
        var r = new INPUT_RECORD();
        r.EventType = 1; r.KeyDown = down ? 1 : 0; r.RepeatCount = 1;
        r.VirtualKeyCode = vk; r.UnicodeChar = ch; r.ControlKeyState = state;
        return r;
    }

    public static string Send(uint pid, ushort vk, char ch, uint state) {
        FreeConsole();
        if (!AttachConsole(pid)) return "AttachConsole failed: " + Marshal.GetLastWin32Error();
        IntPtr h = CreateFileW("CONIN$", 0xC0000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
        uint written;
        var records = new[] { Key(true, vk, ch, state), Key(false, vk, ch, state) };
        bool ok = WriteConsoleInputW(h, records, 2, out written);
        CloseHandle(h);
        FreeConsole();
        return ok ? "ok" : "WriteConsoleInput failed: " + Marshal.GetLastWin32Error();
    }
}
'@

$LEFT_CTRL = 0x0008
foreach ($key in $Keys) {
    if ($key -like 'text:*') {
        foreach ($c in $key.Substring(5).ToCharArray()) {
            $vk = [uint16][char]::ToUpper($c)
            [ConsoleKeys]::Send($ProcessId, $vk, $c, 0) | Out-Null
            Start-Sleep -Milliseconds 15
        }
        continue
    }
    $result = switch ($key) {
        'ctrl+o' { [ConsoleKeys]::Send($ProcessId, 0x4F, [char]0x0F, $LEFT_CTRL) }
        'enter'  { [ConsoleKeys]::Send($ProcessId, 0x0D, [char]13, 0) }
        'f10'    { [ConsoleKeys]::Send($ProcessId, 0x79, [char]0, 0) }
        default  { "unknown key $key" }
    }
    "$key -> $result"
    Start-Sleep -Milliseconds 100
}
