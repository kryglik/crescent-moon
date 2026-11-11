# Sends key presses into the console of a running process (for testing cm's real main loop).
# Usage: sendkey.ps1 -ProcessId <pid> -Keys 'ctrl+o', 'text:hello', 'enter', 'esc', 'alt+f1', 'shift+f4' ...
# From another script (pwsh -File), pass the keys as separate words after the process id:
#   pwsh -File sendkey.ps1 -ProcessId <pid> home down f2
param([int] $ProcessId, [Parameter(ValueFromRemainingArguments)] [string[]] $Keys)

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

# Control key state bits of a console key event.
$LEFT_ALT = 0x0002
$LEFT_CTRL = 0x0008
$SHIFT = 0x0010

# Named keys: virtual key code and the character the console reports with them.
$named = @{
    'esc' = @(0x1B, 27); 'enter' = @(0x0D, 13); 'tab' = @(0x09, 9); 'space' = @(0x20, 32)
    'backspace' = @(0x08, 8); 'insert' = @(0x2D, 0); 'delete' = @(0x2E, 0)
    'up' = @(0x26, 0); 'down' = @(0x28, 0); 'left' = @(0x25, 0); 'right' = @(0x27, 0)
    'home' = @(0x24, 0); 'end' = @(0x23, 0); 'pgup' = @(0x21, 0); 'pgdn' = @(0x22, 0)
}
1..12 | ForEach-Object { $named["f$_"] = @((0x6F + $_), 0) }

foreach ($key in $Keys) {
    if ($key -like 'text:*') {
        foreach ($c in $key.Substring(5).ToCharArray()) {
            # Letters and digits have their own key codes; anything else (like '-', whose code
            # would be Insert's) goes as a bare character.
            $vk = if ($c -match '[a-zA-Z0-9 ]') { [uint16][char]::ToUpper($c) } else { 0 }
            [ConsoleKeys]::Send($ProcessId, $vk, $c, 0) | Out-Null
            Start-Sleep -Milliseconds 15
        }
        continue
    }
    # 'ctrl+alt+shift+NAME', where NAME is one of $named or a single letter or digit.
    $parts = $key.ToLower().Split('+')
    $name = $parts[-1]
    $state = 0
    foreach ($modifier in $parts | Select-Object -SkipLast 1) {
        switch ($modifier) {
            'ctrl' { $state = $state -bor $LEFT_CTRL }
            'alt' { $state = $state -bor $LEFT_ALT }
            'shift' { $state = $state -bor $SHIFT }
        }
    }
    if ($named.ContainsKey($name)) {
        $vk, $code = $named[$name]
        $char = [char]$code
    } elseif ($name.Length -eq 1 -and $name -match '[a-z0-9]') {
        $vk = [int][char]$name.ToUpper()
        # Ctrl+letter arrives as its control code, as from a real keyboard.
        $char = if (($state -band $LEFT_CTRL) -and $name -match '[a-z]') { [char]($vk - 0x40) } else { [char]$name }
    } else {
        "$key -> unknown key"
        continue
    }
    $result = [ConsoleKeys]::Send($ProcessId, [uint16]$vk, $char, [uint32]$state)
    "$key -> $result"
    Start-Sleep -Milliseconds 100
}
