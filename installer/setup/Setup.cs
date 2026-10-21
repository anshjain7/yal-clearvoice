// YAL ClearVoice Setup: one file that installs everything a call-center PC needs.
//
// Install (needs admin; the manifest asks for it once):
//   1. the app (window, engine, model, settings.ini) into a "YAL ClearVoice"
//      folder: in Program Files unless the installer picks another place;
//   2. VB-CABLE, silently, if it isn't installed (bundled with VB-Audio's agreement;
//      Windows still asks once to trust the driver);
//   3. Windows sound: the headset as the speaker, and "CABLE Output" (the cleaned
//      voice) as the default microphone, so calling apps left on their default
//      microphone get the cleaned voice with no settings to change;
//   4. start with Windows, Desktop and Start menu shortcuts, an Apps entry.
// Uninstall reverses it, gives calling apps the headset microphone back, and
// removes VB-CABLE only if this setup installed it. Saved calls are kept.
//
//   "YAL ClearVoice Setup.exe"              the setup window
//   "YAL ClearVoice Setup.exe" /silent      no window (exit code 3010 = restart needed)
//   ... /silent /dir "D:\Apps"              no window, installed in D:\Apps\YAL ClearVoice
//   ... /uninstall [/silent]                remove
//   ... /check <folder>                     diagnostic: unpacks the app there and
//                                           writes check.txt; changes nothing else
//
// .NET Framework 4.8 (part of Windows 10/11). Build: build.ps1 next to this file.

using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.IO;
using System.IO.Compression;
using System.Linq;
using System.Reflection;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Windows.Forms;
using Microsoft.Win32;

namespace YalClearVoice.Setup;

static class Program
{
    [STAThread]
    static int Main(string[] args)
    {
        bool Has(string flag) => args.Any(a => a.Equals(flag, StringComparison.OrdinalIgnoreCase));
        int check = Array.FindIndex(args, a => a.Equals("/check", StringComparison.OrdinalIgnoreCase));
        if (check >= 0 && check + 1 < args.Length) return Installer.Check(args[check + 1]);

        // The copy in the install folder is the uninstaller, however it's opened:
        // from Windows' Apps list (which adds /uninstall) or double-clicked.
        var self = Path.GetFileName(Assembly.GetExecutingAssembly().Location);
        bool uninstall = Has("/uninstall") || self.StartsWith("Uninstall ", StringComparison.OrdinalIgnoreCase);
        Installer.Log($"started as {self} {string.Join(" ", args)}");
        if (Has("/silent"))
        {
            try
            {
                int dir = Array.FindIndex(args, a => a.Equals("/dir", StringComparison.OrdinalIgnoreCase));
                if (!uninstall && dir >= 0 && dir + 1 < args.Length)
                    Installer.Dir = Installer.FolderFor(args[dir + 1]);
                bool reboot = uninstall ? Installer.Uninstall(Installer.Log) : Installer.Install(Installer.Log);
                return reboot ? 3010 : 0;
            }
            catch (Exception e)
            {
                Installer.Log("FAILED: " + e);
                return 1;
            }
        }
        Application.EnableVisualStyles();
        Application.SetCompatibleTextRenderingDefault(false);
        Application.Run(new SetupForm(uninstall));
        return 0;
    }
}

static class Installer
{
    public const string Name = "YAL ClearVoice";
    public const string Version = "0.1.0";
    const string UninstallKey = @"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\YAL ClearVoice";
    // Where the app goes (or is): where it's already installed, else Program Files.
    public static string Dir = Installed()
        ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles), Name);
    public static string AppExe => Path.Combine(Dir, Name + ".exe");
    static string UninstallerExe => Path.Combine(Dir, "Uninstall " + Name + ".exe");

    // The folder of the current installation, if there is one.
    static string? Installed()
    {
        using var u = Registry.LocalMachine.OpenSubKey(UninstallKey);
        return u?.GetValue("InstallLocation") is string dir && dir.Length > 0 ? dir : null;
    }

    // The install folder for a place the installer picked: always a folder of
    // its own named "YAL ClearVoice", so uninstalling never touches their files.
    public static string FolderFor(string picked)
    {
        var full = Path.GetFullPath(picked);
        return Path.GetFileName(full.TrimEnd('\\')).Equals(Name, StringComparison.OrdinalIgnoreCase)
            ? full.TrimEnd('\\')
            : Path.Combine(full, Name);
    }

    // Why `dir` can't take the app, in words for the installer; null if it can.
    public static string? Problem(string dir)
    {
        if (dir.StartsWith(@"\\")) return "Choose a folder on this PC, not a network folder.";
        // Windows can't open the microphone from a program with a very long path.
        if (dir.Length > 150) return "That folder's path is too long. Choose a shorter one, such as C:\\YAL ClearVoice.";
        var root = Path.GetPathRoot(dir);
        if (string.IsNullOrEmpty(root) || new DriveInfo(root).DriveType != DriveType.Fixed)
            return "Choose a folder on one of this PC's own drives.";
        return null;
    }
    const string RunKey = @"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
    static readonly string LogFile = Path.Combine(Path.GetTempPath(), "YAL ClearVoice Setup.log");

    static IEnumerable<string> Shortcuts() => new[]
    {
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.CommonDesktopDirectory), Name + ".lnk"),
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.CommonPrograms), Name + ".lnk"),
    };

    public static void Log(string line)
    {
        try { File.AppendAllText(LogFile, $"{DateTime.Now:yyyy-MM-dd HH:mm:ss}  {line}\r\n"); }
        catch (IOException) { }
    }

    // Installs (or upgrades) everything. `say` reports each step. True if the PC
    // must restart (VB-CABLE was installed).
    public static bool Install(Action<string> say)
    {
        Log($"--- install {Version} into {Dir}");
        if (Problem(Dir) is { } problem) throw new InvalidOperationException(problem);
        say("Closing YAL ClearVoice if it's open…");
        StopApp();

        // Moving an existing installation: take the old copy away first.
        if (Installed() is { } old && !old.Equals(Dir, StringComparison.OrdinalIgnoreCase))
        {
            Log("removing the copy in " + old);
            RemoveFiles(old, Path.Combine(old, "Uninstall " + Name + ".exe"));
        }

        say("Copying YAL ClearVoice…");
        Directory.CreateDirectory(Dir);
        Extract("payload.zip", Dir);
        File.Copy(Assembly.GetExecutingAssembly().Location, UninstallerExe, true);

        bool installedCable = false;
        if (!Audio.HasCable())
        {
            say("Installing VB-CABLE. If Windows asks to install device software, click Install.");
            var pack = Path.Combine(Path.GetTempPath(), "YAL ClearVoice VB-CABLE");
            Extract("vbcable.zip", pack);
            Run(Path.Combine(pack, "VBCABLE_Setup_x64.exe"), "-i -h", pack);
            say("Waiting for VB-CABLE to start…");
            for (int i = 0; i < 30 && !Audio.HasCable(); i++) Thread.Sleep(1000);
            installedCable = true;
            Log("VB-CABLE present after install: " + Audio.HasCable());
        }

        say("Setting up the headset and microphone…");
        Log("sound: " + Audio.UseHeadsetAndCable());

        say("Adding shortcuts…");
        foreach (var lnk in Shortcuts()) Shortcut(lnk, AppExe);
        using (var run = Registry.LocalMachine.CreateSubKey(RunKey))
            run.SetValue(Name, $"\"{AppExe}\"");
        using (var u = Registry.LocalMachine.CreateSubKey(UninstallKey))
        {
            u.SetValue("DisplayName", Name);
            u.SetValue("DisplayVersion", Version);
            u.SetValue("Publisher", "YAL");
            u.SetValue("DisplayIcon", AppExe);
            u.SetValue("InstallLocation", Dir);
            u.SetValue("UninstallString", $"\"{UninstallerExe}\" /uninstall");
            u.SetValue("QuietUninstallString", $"\"{UninstallerExe}\" /uninstall /silent");
            u.SetValue("NoModify", 1, RegistryValueKind.DWord);
            u.SetValue("NoRepair", 1, RegistryValueKind.DWord);
            u.SetValue("EstimatedSize", (int)(DirSize(Dir) / 1024), RegistryValueKind.DWord);
            // Kept across upgrades: uninstall removes VB-CABLE only if we put it there.
            if (installedCable) u.SetValue("InstalledVbCable", 1, RegistryValueKind.DWord);
        }
        Log("installed");
        return installedCable;
    }

    // Removes everything Install added. True if the PC must restart.
    public static bool Uninstall(Action<string> say)
    {
        Log("--- uninstall");
        // No record of the install (it was removed by hand): if this is the
        // uninstaller, its own folder is the install.
        var self = Assembly.GetExecutingAssembly().Location;
        if (Installed() is null && Path.GetFileName(self).Equals(Path.GetFileName(UninstallerExe), StringComparison.OrdinalIgnoreCase))
            Dir = Path.GetDirectoryName(self)!;
        say("Closing YAL ClearVoice…");
        StopApp();

        say("Giving calling apps the headset microphone back…");
        Log("sound: " + Audio.UseHeadsetMicrophone());

        bool removeCable;
        using (var u = Registry.LocalMachine.OpenSubKey(UninstallKey))
            removeCable = u?.GetValue("InstalledVbCable") is int v && v == 1;
        if (removeCable && Audio.HasCable())
        {
            say("Removing VB-CABLE…");
            var pack = Path.Combine(Path.GetTempPath(), "YAL ClearVoice VB-CABLE");
            Extract("vbcable.zip", pack);
            Run(Path.Combine(pack, "VBCABLE_Setup_x64.exe"), "-u -h", pack);
        }

        say("Removing YAL ClearVoice…");
        foreach (var lnk in Shortcuts()) File.Delete(lnk);
        using (var run = Registry.LocalMachine.OpenSubKey(RunKey, true)) run?.DeleteValue(Name, false);
        Registry.LocalMachine.DeleteSubKeyTree(UninstallKey, false);
        // The uninstaller is this running program: it goes once we've exited.
        RemoveFiles(Dir, null);
        DeleteAfterExit(UninstallerExe, Dir);
        Log("uninstalled; VB-CABLE removed: " + removeCable);
        return removeCable;
    }

    // Deletes the app's own files from `dir` (and `uninstaller`, unless null),
    // then the folder only if nothing else is in it. Never deletes anything else.
    static void RemoveFiles(string dir, string? uninstaller)
    {
        using (var zip = new ZipArchive(Resource("payload.zip")))
            foreach (var e in zip.Entries.Where(e => e.Name.Length > 0))
                TryDelete(Path.Combine(dir, e.FullName));
        if (uninstaller is not null) TryDelete(uninstaller);
        try { if (!Directory.EnumerateFileSystemEntries(dir).Any()) Directory.Delete(dir); }
        catch (IOException) { }
    }

    // A running program can't delete itself: a hidden PowerShell waits for this
    // process to exit (however long its window stays open), then deletes
    // `uninstaller` and `dir`, the folder only if nothing else is in it.
    public static void DeleteAfterExit(string uninstaller, string dir)
    {
        static string Quote(string s) => "'" + s.Replace("'", "''") + "'";
        var script =
            $"Wait-Process -Id {Process.GetCurrentProcess().Id} -ErrorAction SilentlyContinue; " +
            $"Remove-Item -LiteralPath {Quote(uninstaller)} -Force -ErrorAction SilentlyContinue; " +
            $"if (-not (Get-ChildItem -LiteralPath {Quote(dir)} -Force -ErrorAction SilentlyContinue)) " +
            $"{{ Remove-Item -LiteralPath {Quote(dir)} -Force -ErrorAction SilentlyContinue }}";
        var encoded = Convert.ToBase64String(Encoding.Unicode.GetBytes(script));
        Process.Start(new ProcessStartInfo("powershell.exe",
            $"-NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand {encoded}")
        {
            CreateNoWindow = true,
            UseShellExecute = false,
        });
    }

    static void TryDelete(string file)
    {
        try { File.Delete(file); }
        catch (Exception e) when (e is IOException || e is UnauthorizedAccessException) { Log($"couldn't delete {file}: {e.Message}"); }
    }

    // Diagnostic without admin or changes: unpacks the app into `folder` and
    // reports what Install would find and do there.
    public static int Check(string folder)
    {
        var report = new StringBuilder();
        try
        {
            Directory.CreateDirectory(folder);
            Extract("payload.zip", folder);
            foreach (var f in Directory.GetFiles(folder))
                report.AppendLine($"unpacked: {Path.GetFileName(f)} ({new FileInfo(f).Length:N0} bytes)");
            using (var zip = new ZipArchive(Resource("vbcable.zip")))
                report.AppendLine($"VB-CABLE pack: {zip.Entries.Count} files, setup present: "
                    + zip.Entries.Any(e => e.Name == "VBCABLE_Setup_x64.exe"));
            report.AppendLine("default install folder: " + Dir);
            foreach (var picked in new[] { @"C:\Program Files", @"C:\", @"C:\Apps\YAL ClearVoice\", @"\\server\share", @"C:\" + new string('x', 160) })
            {
                var dir = FolderFor(picked);
                report.AppendLine($"picked {(picked.Length > 40 ? picked.Substring(0, 40) + "…" : picked)} -> {(dir.Length > 60 ? dir.Substring(0, 60) + "…" : dir)}: {Problem(dir) ?? "ok"}");
            }
            report.AppendLine("VB-CABLE installed: " + Audio.HasCable());
            report.AppendLine(Audio.Describe());
            report.AppendLine("Install would set: " + Audio.Plan());
            report.AppendLine("sound settings API available: " + Audio.CanSetDefaults());
            Shortcut(Path.Combine(folder, "test shortcut.lnk"), Path.Combine(folder, Name + ".exe"));
            report.AppendLine("shortcut created: " + File.Exists(Path.Combine(folder, "test shortcut.lnk")));
        }
        catch (Exception e)
        {
            report.AppendLine("FAILED: " + e);
        }
        File.WriteAllText(Path.Combine(folder, "check.txt"), report.ToString(), new UTF8Encoding(true));
        return 0;
    }

    static void StopApp()
    {
        foreach (var name in new[] { Name, Name + " Engine" })
            foreach (var p in Process.GetProcessesByName(name))
            {
                try { p.Kill(); p.WaitForExit(5000); }
                catch (Exception) { }
            }
    }

    static Stream Resource(string name) =>
        Assembly.GetExecutingAssembly().GetManifestResourceStream(name)
        ?? throw new InvalidOperationException(name + " is missing from this setup.");

    static void Extract(string resource, string dir)
    {
        var root = Path.GetFullPath(dir).TrimEnd('\\') + "\\";
        using var zip = new ZipArchive(Resource(resource));
        foreach (var e in zip.Entries)
        {
            if (e.Name.Length == 0) continue; // a folder entry
            var path = Path.GetFullPath(Path.Combine(root, e.FullName));
            if (!path.StartsWith(root, StringComparison.OrdinalIgnoreCase)) continue;
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            e.ExtractToFile(path, true);
        }
    }

    static void Run(string exe, string args, string dir)
    {
        Log($"run: {Path.GetFileName(exe)} {args}");
        using var p = Process.Start(new ProcessStartInfo(exe, args) { WorkingDirectory = dir, UseShellExecute = false });
        if (!p.WaitForExit(5 * 60 * 1000)) Log("still running after 5 minutes; continuing");
        else Log("exit code " + p.ExitCode);
    }

    static void Shortcut(string lnk, string target)
    {
        dynamic shell = Activator.CreateInstance(Type.GetTypeFromProgID("WScript.Shell")!)!;
        dynamic s = shell.CreateShortcut(lnk);
        s.TargetPath = target;
        s.WorkingDirectory = Path.GetDirectoryName(target);
        s.Description = "Noise removal, live transcripts and call notes for calls";
        s.Save();
    }

    static long DirSize(string dir) =>
        Directory.GetFiles(dir, "*", SearchOption.AllDirectories).Sum(f => new FileInfo(f).Length);
}

/// Windows' audio devices: which exist, which are the defaults, and setting the
/// defaults (through IPolicyConfig, what the Sound control panel itself uses).
static class Audio
{
    const int Render = 0, Capture = 1, Active = 1;
    static readonly string[] Virtual = { "VB-Audio", "CABLE", "SoliCall", "YAL ClearVoice" };
    // Headsets agents wear, preferred over the PC's own speakers and microphone.
    static readonly string[] Headsets = { "Detecta", "Headset", "Headphone", "Earphone" };

    sealed class Device
    {
        public Device(string id, string name)
        {
            Id = id;
            Name = name;
        }
        public string Id { get; }
        public string Name { get; }
    }

    static bool IsVirtual(string name) => Virtual.Any(name.Contains);
    static bool IsHeadset(string name) => Headsets.Any(name.Contains);

    public static bool HasCable() => Devices(Render).Any(d => d.Name.Contains("CABLE Input"))
        && Devices(Capture).Any(d => d.Name.Contains("CABLE Output"));

    // The real speaker to use: a headset (Windows' current default first), else
    // the current default if real, else any real one.
    static Device? Speaker()
    {
        var current = DefaultId(Render);
        return Devices(Render).Where(d => !IsVirtual(d.Name))
            .OrderByDescending(d => IsHeadset(d.Name)).ThenByDescending(d => d.Id == current)
            .FirstOrDefault();
    }

    static Device? RealMicrophone()
    {
        var current = DefaultId(Capture);
        return Devices(Capture).Where(d => !IsVirtual(d.Name))
            .OrderByDescending(d => IsHeadset(d.Name)).ThenByDescending(d => d.Id == current)
            .FirstOrDefault();
    }

    static Device? CableOutput() => Devices(Capture).FirstOrDefault(d => d.Name.Contains("CABLE Output"));

    public static string Plan() =>
        $"speaker = {Speaker()?.Name ?? "(none)"}, microphone = {CableOutput()?.Name ?? "(VB-CABLE not installed yet)"}";

    // Install: the headset as the speaker, the cleaned voice as the microphone.
    public static string UseHeadsetAndCable()
    {
        var notes = new List<string>();
        if (Speaker() is { } s) { SetDefault(s.Id); notes.Add("speaker = " + s.Name); }
        if (CableOutput() is { } c) { SetDefault(c.Id); notes.Add("microphone = " + c.Name); }
        else notes.Add("CABLE Output not found; microphone left as is");
        return string.Join("; ", notes);
    }

    // Uninstall: if calling apps' default microphone is the cable, give them a real one.
    public static string UseHeadsetMicrophone()
    {
        var current = DefaultId(Capture);
        if (current is null || CableOutput()?.Id != current) return "microphone left as is";
        if (RealMicrophone() is not { } m) return "no real microphone found";
        SetDefault(m.Id);
        return "microphone = " + m.Name;
    }

    public static string Describe()
    {
        var sb = new StringBuilder();
        foreach (var (flow, label) in new[] { (Render, "speaker"), (Capture, "microphone") })
        {
            var current = DefaultId(flow);
            foreach (var d in Devices(flow))
                sb.AppendLine($"{label}: {d.Name}{(d.Id == current ? "  [default]" : "")}");
        }
        return sb.ToString().TrimEnd();
    }

    public static bool CanSetDefaults()
    {
        try { return new PolicyConfigClient() is IPolicyConfig; }
        catch (Exception) { return false; }
    }

    static void SetDefault(string id)
    {
        var policy = (IPolicyConfig)new PolicyConfigClient();
        for (int role = 0; role < 3; role++) // console, multimedia, communications
            Marshal.ThrowExceptionForHR(policy.SetDefaultEndpoint(id, role));
    }

    static List<Device> Devices(int flow)
    {
        var list = new List<Device>();
        var e = (IMMDeviceEnumerator)new MMDeviceEnumerator();
        if (e.EnumAudioEndpoints(flow, Active, out var all) != 0) return list;
        all.GetCount(out int n);
        for (int i = 0; i < n; i++)
            if (all.Item(i, out var d) == 0 && d.GetId(out var id) == 0)
                list.Add(new Device(id, FriendlyName(d)));
        return list;
    }

    static string? DefaultId(int flow)
    {
        var e = (IMMDeviceEnumerator)new MMDeviceEnumerator();
        return e.GetDefaultAudioEndpoint(flow, 0, out var d) == 0 && d.GetId(out var id) == 0 ? id : null;
    }

    static string FriendlyName(IMMDevice d)
    {
        if (d.OpenPropertyStore(0, out var store) != 0) return "";
        var key = new PropertyKey { fmtid = new Guid("a45c254e-df1c-4efd-8020-67d146a850e0"), pid = 14 };
        if (store.GetValue(ref key, out var v) != 0) return "";
        try { return v.vt == 31 ? Marshal.PtrToStringUni(v.pointer) ?? "" : ""; } // VT_LPWSTR
        finally { PropVariantClear(ref v); }
    }

    [DllImport("ole32.dll")]
    static extern int PropVariantClear(ref PropVariant pvar);

    // ---- COM declarations (mmdeviceapi.h; IPolicyConfig is undocumented but
    // stable since Windows 7). Only the methods up to the last one called matter.

    [ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")]
    class MMDeviceEnumerator { }

    [ComImport, Guid("870af99c-171d-4f9e-af0d-e63df40c2bc9")]
    class PolicyConfigClient { }

    [ComImport, Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IMMDeviceEnumerator
    {
        [PreserveSig] int EnumAudioEndpoints(int dataFlow, int stateMask, out IMMDeviceCollection devices);
        [PreserveSig] int GetDefaultAudioEndpoint(int dataFlow, int role, out IMMDevice device);
    }

    [ComImport, Guid("0BD7A1BE-7A1A-44DB-8397-CC5392387B5E"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IMMDeviceCollection
    {
        [PreserveSig] int GetCount(out int count);
        [PreserveSig] int Item(int index, out IMMDevice device);
    }

    [ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IMMDevice
    {
        [PreserveSig] int Activate(ref Guid iid, int clsCtx, IntPtr parameters, [MarshalAs(UnmanagedType.IUnknown)] out object iface);
        [PreserveSig] int OpenPropertyStore(int access, out IPropertyStore store);
        [PreserveSig] int GetId([MarshalAs(UnmanagedType.LPWStr)] out string id);
    }

    [ComImport, Guid("886d8eeb-8cf2-4446-8d02-cdba1dbdcf99"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IPropertyStore
    {
        [PreserveSig] int GetCount(out int count);
        [PreserveSig] int GetAt(int index, out PropertyKey key);
        [PreserveSig] int GetValue(ref PropertyKey key, out PropVariant value);
    }

    [ComImport, Guid("f8679f50-850a-41cf-9c72-430f290290c8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IPolicyConfig
    {
        [PreserveSig] int GetMixFormat([MarshalAs(UnmanagedType.LPWStr)] string id, IntPtr format);
        [PreserveSig] int GetDeviceFormat([MarshalAs(UnmanagedType.LPWStr)] string id, int isDefault, IntPtr format);
        [PreserveSig] int ResetDeviceFormat([MarshalAs(UnmanagedType.LPWStr)] string id);
        [PreserveSig] int SetDeviceFormat([MarshalAs(UnmanagedType.LPWStr)] string id, IntPtr endpointFormat, IntPtr mixFormat);
        [PreserveSig] int GetProcessingPeriod([MarshalAs(UnmanagedType.LPWStr)] string id, int isDefault, IntPtr defaultPeriod, IntPtr minimumPeriod);
        [PreserveSig] int SetProcessingPeriod([MarshalAs(UnmanagedType.LPWStr)] string id, IntPtr period);
        [PreserveSig] int GetShareMode([MarshalAs(UnmanagedType.LPWStr)] string id, IntPtr mode);
        [PreserveSig] int SetShareMode([MarshalAs(UnmanagedType.LPWStr)] string id, IntPtr mode);
        [PreserveSig] int GetPropertyValue([MarshalAs(UnmanagedType.LPWStr)] string id, int fxStore, IntPtr key, IntPtr value);
        [PreserveSig] int SetPropertyValue([MarshalAs(UnmanagedType.LPWStr)] string id, int fxStore, IntPtr key, IntPtr value);
        [PreserveSig] int SetDefaultEndpoint([MarshalAs(UnmanagedType.LPWStr)] string id, int role);
        [PreserveSig] int SetEndpointVisibility([MarshalAs(UnmanagedType.LPWStr)] string id, int visible);
    }

    [StructLayout(LayoutKind.Sequential)]
    struct PropertyKey
    {
        public Guid fmtid;
        public int pid;
    }

    [StructLayout(LayoutKind.Explicit, Size = 24)]
    struct PropVariant
    {
        [FieldOffset(0)] public short vt;
        [FieldOffset(8)] public IntPtr pointer;
    }
}

/// The setup window: what will happen, one button, progress, and the finish
/// (restart now, or open the app).
sealed class SetupForm : Form
{
    static readonly Color Teal = Color.FromArgb(0x00, 0x98, 0x9C);
    static readonly Color Ink = Color.FromArgb(0x1A, 0x26, 0x26);
    static readonly Color Muted = Color.FromArgb(0x5C, 0x6B, 0x6B);

    readonly bool _uninstall;
    readonly Label _status = new() { AutoSize = true, ForeColor = Muted, Text = "" };
    readonly ProgressBar _bar = new() { Style = ProgressBarStyle.Marquee, Width = 440, Height = 8, Visible = false };
    readonly Button _go = new() { Size = new Size(180, 34), FlatStyle = FlatStyle.Flat, BackColor = Teal, ForeColor = Color.White };
    readonly Button _close = new() { Text = "Cancel", Size = new Size(110, 34) };
    readonly TextBox _folder = new() { ReadOnly = true, Width = 330, Margin = new Padding(0, 3, 8, 0) };
    readonly Button _change = new() { Text = "Change…", AutoSize = true, Margin = Padding.Empty };
    Action? _next; // what _go does after the work is done
    bool _busy;

    static Stream Resource(string name) =>
        Assembly.GetExecutingAssembly().GetManifestResourceStream(name)
        ?? throw new InvalidOperationException(name + " is missing from this setup.");

    public SetupForm(bool uninstall)
    {
        _uninstall = uninstall;
        SuspendLayout();
        AutoScaleDimensions = new SizeF(96F, 96F);
        AutoScaleMode = AutoScaleMode.Dpi;
        Text = uninstall ? "Uninstall YAL ClearVoice" : "YAL ClearVoice Setup";
        using (var icon = Resource("yal.ico")) Icon = new Icon(icon);
        ClientSize = new Size(500, uninstall ? 340 : 410);
        BackColor = Color.FromArgb(0xFC, 0xFC, 0xFC);
        ForeColor = Ink;
        Font = new Font("Segoe UI", 9.5F);
        FormBorderStyle = FormBorderStyle.FixedDialog;
        MaximizeBox = MinimizeBox = false;
        StartPosition = FormStartPosition.CenterScreen;

        var flow = new FlowLayoutPanel
        {
            Dock = DockStyle.Fill, FlowDirection = FlowDirection.TopDown, WrapContents = false,
            Padding = new Padding(28, 24, 28, 16),
        };
        // YAL's logo, then the product name.
        var header = new FlowLayoutPanel { AutoSize = true, FlowDirection = FlowDirection.LeftToRight, Margin = new Padding(0, 0, 0, 14) };
        using (var logo = Resource("yal-logo.png"))
            header.Controls.Add(new PictureBox
            {
                Image = new Bitmap(Image.FromStream(logo)), SizeMode = PictureBoxSizeMode.Zoom,
                Size = new Size(84, 31), Margin = new Padding(0, 0, 6, 0),
            });
        header.Controls.Add(new Label
        {
            Text = "ClearVoice", AutoSize = true, ForeColor = Ink,
            Font = new Font("Segoe UI Semibold", 15F), Margin = new Padding(0, 2, 0, 0),
        });
        flow.Controls.Add(header);
        // Messages carry their own line breaks (a wrapping label scales badly with DPI).
        flow.Controls.Add(new Label
        {
            AutoSize = true, Margin = new Padding(0, 0, 0, 16),
            Text = uninstall
                ? "Uninstall YAL ClearVoice from this PC?\n\n"
                  + "This removes the app and gives your calling apps the\n"
                  + "headset microphone back. Saved calls are kept."
                : "This installs, in one go:\n"
                  + "  •  the YAL ClearVoice app, which starts with Windows\n"
                  + "  •  VB-CABLE, the virtual microphone it uses (if it's missing)\n"
                  + "  •  your headset as the speaker, and YAL ClearVoice's cleaned\n"
                  + "      voice as the microphone for your calling apps\n\n"
                  + "Plug in the headset before you start.",
        });
        if (!uninstall)
        {
            flow.Controls.Add(new Label { Text = "Install to:", AutoSize = true, Margin = new Padding(0, 0, 0, 4) });
            var row = new FlowLayoutPanel { AutoSize = true, FlowDirection = FlowDirection.LeftToRight, Margin = new Padding(0, 0, 0, 8) };
            row.Controls.Add(_folder);
            row.Controls.Add(_change);
            flow.Controls.Add(row);
            _folder.Text = Installer.Dir;
        }
        flow.Controls.Add(_bar);
        _status.Margin = new Padding(0, 8, 0, 16);
        flow.Controls.Add(_status);
        var buttons = new FlowLayoutPanel { AutoSize = true, FlowDirection = FlowDirection.LeftToRight, Margin = Padding.Empty };
        buttons.Controls.Add(_go);
        buttons.Controls.Add(_close);
        flow.Controls.Add(buttons);
        Controls.Add(flow);
        ResumeLayout();

        _go.Text = uninstall ? "Uninstall" : "Install";
        ActiveControl = _go; // not the folder box, whose text would show selected
        _folder.TabStop = false;
        _go.Click += (_, _) => { if (_next is { } next) next(); else Start(); };
        _change.Click += (_, _) => PickFolder();
        _close.Click += (_, _) => Close();
        FormClosing += (_, e) => e.Cancel = _busy;
    }

    // "Change…": the installer picks a place; the app gets its own folder there.
    void PickFolder()
    {
        using var dialog = new FolderBrowserDialog
        {
            Description = "Choose where to install YAL ClearVoice. It goes in a \"YAL ClearVoice\" folder there.",
            ShowNewFolderButton = true,
            SelectedPath = Path.GetDirectoryName(Installer.Dir) ?? "",
        };
        if (dialog.ShowDialog(this) != DialogResult.OK) return;
        var dir = Installer.FolderFor(dialog.SelectedPath);
        if (Installer.Problem(dir) is { } problem)
        {
            _status.ForeColor = Ink;
            _status.Text = problem;
            return;
        }
        Installer.Dir = dir;
        _folder.Text = dir;
        _status.ForeColor = Muted;
        _status.Text = "";
    }

    void Start()
    {
        _busy = true;
        _go.Enabled = _close.Enabled = _change.Enabled = false;
        _bar.Visible = true;
        void Say(string s)
        {
            Installer.Log(s);
            Ui(() => _status.Text = s);
        }
        new Thread(() =>
        {
            try
            {
                bool reboot = _uninstall ? Installer.Uninstall(Say) : Installer.Install(Say);
                Ui(() => Finish(reboot));
            }
            catch (Exception e)
            {
                Installer.Log("FAILED: " + e);
                Ui(() => Fail(e));
            }
        }) { IsBackground = true }.Start();
    }

    void Finish(bool reboot)
    {
        _busy = false;
        _bar.Visible = false;
        _go.Enabled = _close.Enabled = true;
        if (reboot)
        {
            _status.Text = (_uninstall ? "YAL ClearVoice has been uninstalled." : "YAL ClearVoice is installed.")
                + "\nRestart the PC to finish setting up VB-CABLE.";
            _go.Text = "Restart now";
            _close.Text = "Restart later";
            _next = () => Process.Start(new ProcessStartInfo("shutdown.exe", "/r /t 0") { CreateNoWindow = true, UseShellExecute = false });
        }
        else if (_uninstall)
        {
            _status.Text = "YAL ClearVoice has been uninstalled.";
            _go.Visible = false;
            _close.Text = "Close";
        }
        else
        {
            _status.Text = "YAL ClearVoice is installed.";
            _go.Text = "Open YAL ClearVoice";
            _close.Text = "Close";
            // Through Explorer, so the app runs as the signed-in user, not as admin.
            _next = () =>
            {
                Process.Start("explorer.exe", $"\"{Installer.AppExe}\"");
                Close();
            };
        }
    }

    void Fail(Exception e)
    {
        _busy = false;
        _bar.Visible = false;
        _close.Enabled = true;
        _close.Text = "Close";
        _status.ForeColor = Ink;
        _status.Text = "Setup couldn't finish: " + e.Message
            + "\nDetails are in \"YAL ClearVoice Setup.log\" in the Temp folder.";
    }

    void Ui(Action a)
    {
        if (IsHandleCreated && !IsDisposed) BeginInvoke(a);
    }
}
