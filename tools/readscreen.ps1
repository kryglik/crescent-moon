# Prints the visible text of another process's console window (for testing cm in Windows Terminal).
param([int] $ProcessId)

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;

public static class ConsoleScreen {
    [StructLayout(LayoutKind.Sequential)] public struct COORD { public short X, Y; }
    [StructLayout(LayoutKind.Sequential)] public struct SMALL_RECT { public short Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct CSBI {
        public COORD Size; public COORD Cursor; public ushort Attributes; public SMALL_RECT Window; public COORD MaxSize;
    }
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool FreeConsole();
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool AttachConsole(uint pid);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    public static extern IntPtr CreateFileW(string name, uint access, uint share, IntPtr sa, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool GetConsoleScreenBufferInfo(IntPtr h, out CSBI info);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    public static extern bool ReadConsoleOutputCharacterW(IntPtr h, StringBuilder text, uint length, COORD at, out uint read);

    public static string Read(uint pid) {
        FreeConsole();
        if (!AttachConsole(pid)) return "AttachConsole failed: " + Marshal.GetLastWin32Error();
        IntPtr h = CreateFileW("CONOUT$", 0xC0000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
        CSBI info;
        GetConsoleScreenBufferInfo(h, out info);
        var all = new StringBuilder();
        all.AppendFormat("window {0}x{1}, cursor {2},{3}\n", info.Window.Right - info.Window.Left + 1,
            info.Window.Bottom - info.Window.Top + 1, info.Cursor.X, info.Cursor.Y - info.Window.Top);
        int width = info.Window.Right - info.Window.Left + 1;
        for (short y = info.Window.Top; y <= info.Window.Bottom; y++) {
            var line = new StringBuilder(width);
            uint read;
            ReadConsoleOutputCharacterW(h, line, (uint)width, new COORD { X = info.Window.Left, Y = y }, out read);
            all.AppendLine(line.ToString(0, (int)read).TrimEnd());
        }
        CloseHandle(h);
        FreeConsole();
        return all.ToString();
    }
}
'@
[ConsoleScreen]::Read($ProcessId)
