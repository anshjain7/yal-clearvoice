// YAL ClearVoice UI — the control window for the audio engine.
//
// It does NO audio processing. On start it launches the engine
// ("YAL ClearVoice Engine.exe --headless", in the same folder) and talks to it over
// the local pipe \\.\pipe\YalClearVoice: one command per line, one JSON reply. A
// background worker owns the pipe (connect, poll ~4 Hz, reconnect); the window only
// renders status and queues commands. Closing the window stops the engine.
//
// Calls: the engine transcribes both sides live (Sarvam AI); the window shows the
// transcript and, when a call ends, asks the summary server in settings.ini
// ([summary]: an OpenAI-compatible chat endpoint) for after-call notes. Unless
// "Save calls" is off, each finished call is saved on this PC (CallStore) and can
// be read again under "Previous calls" (HistoryForm).
//
// Targets .NET Framework 4.8, which is part of Windows 10/11, so there's no runtime
// to install or bundle. Build: build.ps1.

using System;
using System.Collections;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.Globalization;
using System.IO;
using System.IO.Pipes;
using System.Linq;
using System.Net;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Web.Script.Serialization;
using System.Windows.Forms;
using Microsoft.Win32;

namespace YalClearVoice.Ui;

static class Program
{
    [DllImport("user32.dll")]
    static extern bool SetProcessDPIAware();

    [STAThread]
    static void Main()
    {
        // One window at a time; a second launch just exits.
        using var single = new Mutex(true, @"Local\YalClearVoiceUi", out bool first);
        if (!first) return;
        SetProcessDPIAware(); // sharp text on scaled screens; the form scales its layout
        Application.EnableVisualStyles();
        Application.SetCompatibleTextRenderingDefault(false);
        Application.Run(new MainForm());
    }
}

/// YAL's logo and icon, built into the exe from ui\brand (see make-icons.ps1).
static class Brand
{
    static Stream Resource(string name) =>
        typeof(Brand).Assembly.GetManifestResourceStream(name)
        ?? throw new InvalidOperationException(name + " is missing from the build.");

    public static Icon AppIcon()
    {
        using var s = Resource("yal.ico");
        return new Icon(s);
    }

    // The full logo, drawn 4x size so it stays sharp on scaled screens.
    public static Image LogoImage()
    {
        using var s = Resource("yal-logo.png");
        return new Bitmap(Image.FromStream(s));
    }
}

/// One device choice in a dropdown. Id is null for "Automatic".
sealed class DeviceItem
{
    public DeviceItem(string? id, string name)
    {
        Id = id;
        Name = name;
    }
    public string? Id { get; }
    public string Name { get; }
    public override string ToString() => Name;
}

sealed class MainForm : Form
{
    const string EngineName = "YAL ClearVoice Engine";

    // YAL brand palette (yal.inc).
    static readonly Color Teal = Color.FromArgb(0x00, 0x98, 0x9C);
    static readonly Color Bg = Color.FromArgb(0xFC, 0xFC, 0xFC);
    static readonly Color Panel = Color.FromArgb(0xF7, 0xFA, 0xFA);
    static readonly Color Ink = Color.FromArgb(0x1A, 0x26, 0x26);
    static readonly Color Muted = Color.FromArgb(0x5C, 0x6B, 0x6B);

    // Used only by the worker thread.
    static readonly JavaScriptSerializer Json = new();

    readonly ComboBox _mic = new() { DropDownStyle = ComboBoxStyle.DropDownList };
    readonly ComboBox _spk = new() { DropDownStyle = ComboBoxStyle.DropDownList };
    readonly CheckBox _nc = new() { Text = "Noise cancellation", AutoSize = true };
    // WeSep on the live call, before noise removal (a test: the agent's voice
    // reaches the customer seconds late). Shown when the engine has WeSep.
    readonly CheckBox _liveTse = new() { Text = "Voice filter (WeSep)", AutoSize = true, Visible = false };
    readonly Label _liveTseStatus = new() { AutoSize = true, ForeColor = Muted };
    // The agent's choice (UI thread; null until the engine first reports, then
    // the engine's own, on by default). Kept apart from the box: the engine's
    // status mustn't untick it while the warning is open, and the switch is
    // sent again if the engine reports otherwise (restarted, or not there yet).
    bool? _liveTseWanted;
    readonly Label _headset = new();
    readonly Label _conn = new();
    readonly Label _dfn = new();
    readonly Label _latency = new();
    readonly Label _hint = new();
    readonly Label _callState = new() { AutoSize = true };
    readonly RichTextBox _transcript = new()
    {
        ReadOnly = true, BorderStyle = BorderStyle.None, BackColor = Panel, DetectUrls = false,
        ScrollBars = RichTextBoxScrollBars.Vertical,
    };
    readonly Label _live = new() { AutoSize = false, ForeColor = Muted };
    readonly Button _transcribe = new() { Text = "Start transcription", Enabled = false };
    bool _transcribing; // UI thread: the engine reports a call on
    readonly TextBox _summary = new()
    {
        Multiline = true, ReadOnly = true, BorderStyle = BorderStyle.None, BackColor = Panel,
        ScrollBars = ScrollBars.Vertical,
    };
    readonly Button _copy = new() { Text = "Copy" };
    readonly Button _history = new() { Text = "Previous calls" };
    readonly CheckBox _save = new() { Text = "Save calls", AutoSize = true };
    volatile bool _saveCalls;
    readonly Label _notesPath = new() { AutoSize = false, AutoEllipsis = true };
    readonly Button _notesChange = new() { Text = "Change…" };
    readonly Button _notesOpen = new() { Text = "Open folder" };
    readonly Label _noteStatus = new() { AutoSize = true, ForeColor = Muted };
    readonly CheckBox _record = new() { Text = "Record call audio", AutoSize = true };
    readonly Label _recStatus = new() { AutoSize = true, ForeColor = Muted };
    readonly Button _recOpen = new() { Text = "Open recordings" };
    // The agent's voice sample, for keeping other voices out of the transcript
    // (shown only when the engine has a voice server in settings.ini).
    readonly Button _voiceRec = new() { Text = "Register my voice", Visible = false };
    readonly Button _voiceFile = new() { Text = "Upload voice...", Visible = false };
    readonly Label _voiceStatus = new() { AutoSize = true, ForeColor = Muted, Visible = false };
    volatile bool _recordWanted;   // "Record call audio" ticked (and agreed to)
    int _recordingCall;            // worker thread: the call being recorded, 0 = none
    HistoryForm? _historyForm;
    Font? _bold;

    readonly ConcurrentQueue<string> _commands = new();
    Process? _engine;
    volatile bool _running = true;
    bool _loading;   // suppresses control events while applying engine state
    bool _spkLocked; // output picker fixed to VB-CABLE

    // The call the window shows, owned by the worker thread: its id and its
    // finished lines ("Customer: …" / "Agent: …"), kept for the summary.
    int _callId;
    DateTime _callStart = DateTime.Now;
    readonly List<string> _lines = new();
    string _deviceKey = ""; // the device list the pickers show (worker thread)
    int _summarizedCall;
    volatile bool _summarizing;
    // Finished calls, by id: when each began and ended, for an improved
    // transcript that comes after the call (worker thread). And the last call
    // summarized again from one.
    readonly Dictionary<int, (DateTime Start, DateTime End)> _ended = new();
    int _improvedCall;

    public MainForm()
    {
        SuspendLayout();
        // Positions below are 96-DPI pixels; WinForms scales them to the screen.
        AutoScaleDimensions = new SizeF(96F, 96F);
        AutoScaleMode = AutoScaleMode.Dpi;
        Text = "YAL ClearVoice";
        Icon = Brand.AppIcon();
        ClientSize = new Size(920, 576 + HeaderHeight);
        BackColor = Bg;
        ForeColor = Ink;
        Font = new Font("Segoe UI", 9F);
        FormBorderStyle = FormBorderStyle.FixedSingle;
        MaximizeBox = false;
        BuildLayout();
        ResumeLayout();

        _mic.SelectedIndexChanged += (_, _) => OnDevicePicked(_mic, "mic");
        _spk.SelectedIndexChanged += (_, _) => OnDevicePicked(_spk, "spk");
        _nc.CheckedChanged += (_, _) =>
        {
            if (!_loading) _commands.Enqueue(_nc.Checked ? "nc on" : "nc off");
        };
        _liveTse.CheckedChanged += (_, _) =>
        {
            if (_loading) return;
            if (_liveTse.Checked && MessageBox.Show(this, LiveTsePrompt(_liveTseDelayMs), "Voice filter (WeSep)",
                    MessageBoxButtons.OKCancel, MessageBoxIcon.Warning) != DialogResult.OK)
            {
                _loading = true;
                _liveTse.Checked = false;
                _loading = false;
                return;
            }
            _liveTseWanted = _liveTse.Checked;
            _commands.Enqueue(_liveTse.Checked ? "tse on" : "tse off");
        };
        // The agent starts and stops transcription. Stopping ends the call, and
        // the worker then writes its summary.
        _transcribe.Click += (_, _) =>
        {
            _commands.Enqueue(_transcribing ? "endcall" : "startcall");
            _transcribe.Enabled = false; // until the engine confirms
        };
        _copy.Click += (_, _) =>
        {
            if (_summary.Text.Length == 0) return;
            Clipboard.SetText(_summary.Text);
            _copy.Text = "Copied";
            var t = new System.Windows.Forms.Timer { Interval = 1500 };
            t.Tick += (_, _) => { _copy.Text = "Copy"; t.Dispose(); };
            t.Start();
        };
        _save.Checked = _saveCalls = CallStore.Enabled;
        _save.CheckedChanged += (_, _) => CallStore.Enabled = _saveCalls = _save.Checked;
        _history.Click += (_, _) =>
        {
            if (_historyForm is { IsDisposed: false }) _historyForm.Activate();
            else (_historyForm = new HistoryForm(Bg, Panel)).Show(this);
        };

        _notesPath.Text = Notes.Folder;
        _notesChange.Click += (_, _) =>
        {
            using var dialog = new FolderBrowserDialog
            {
                Description = "Choose the folder where summary notes are saved.",
                ShowNewFolderButton = true,
                SelectedPath = Notes.Folder,
            };
            if (dialog.ShowDialog(this) != DialogResult.OK) return;
            Notes.Folder = dialog.SelectedPath;
            _notesPath.Text = Notes.Folder;
        };
        _notesOpen.Click += (_, _) => OpenFolder(Notes.Folder);

        // Recording needs the agent's agreement each time it's switched on.
        _record.CheckedChanged += (_, _) =>
        {
            if (_loading) return;
            if (_record.Checked && MessageBox.Show(this, RecordConsent, "Record call audio",
                    MessageBoxButtons.YesNo, MessageBoxIcon.Information) != DialogResult.Yes)
            {
                _loading = true;
                _record.Checked = false;
                _loading = false;
            }
            _recordWanted = _record.Checked;
            _recStatus.Text = _recordWanted ? "On: records each transcribed call" : "";
        };
        _recOpen.Click += (_, _) => OpenFolder(Recordings);
        _voiceRec.Click += (_, _) =>
        {
            if (MessageBox.Show(this, VoicePrompt, "Register my voice",
                    MessageBoxButtons.OKCancel, MessageBoxIcon.Information) != DialogResult.OK) return;
            _commands.Enqueue("voice record");
            _voiceStatus.Text = "● Speak now, for 20 seconds";
            _voiceStatus.ForeColor = Teal;
        };
        _voiceFile.Click += (_, _) =>
        {
            if (MessageBox.Show(this, UploadPrompt, "Upload my voice",
                    MessageBoxButtons.OKCancel, MessageBoxIcon.Information) != DialogResult.OK) return;
            using var pick = new OpenFileDialog
            {
                Title = "Choose a recording of your voice",
                InitialDirectory = Environment.GetFolderPath(Environment.SpecialFolder.Desktop),
                Filter = "Audio files|*.wav;*.mp3;*.m4a;*.aac;*.wma;*.flac|All files|*.*",
            };
            if (pick.ShowDialog(this) != DialogResult.OK) return;
            _commands.Enqueue("voice import " + pick.FileName);
            _voiceStatus.Text = "Reading the file...";
            _voiceStatus.ForeColor = Teal;
        };

        StartEngine();
        // Start talking to the engine once the window exists (UI updates need it).
        Shown += (_, _) => new Thread(Worker) { IsBackground = true }.Start();
        FormClosing += (_, _) =>
        {
            _running = false;
            StopEngines();
        };
    }

    // Call recordings: private to the Windows user, not synced by OneDrive.
    static readonly string Recordings = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "YAL ClearVoice", "Recordings");

    const string VoicePrompt =
        "Register your voice so YAL ClearVoice keeps other people's voices out of your "
        + "transcripts. It needs 20 seconds of you talking.\n\n"
        + "After you click OK, talk normally for 20 seconds, as on a call. For example, "
        + "say your name, your team, and read out a phone number. Pick a moment when "
        + "people near you aren't talking.\n\n"
        + "Your voice sample is kept on this PC, for your Windows account only, and used "
        + "only to recognise your voice in transcripts. You can register again at any time.";

    // How far behind the microphone the voice filter plays (UI thread; from
    // the engine: about 0.7 s with a voice server, 3 s on this PC).
    long _liveTseDelayMs = 3000;

    static string LiveTsePrompt(long delayMs) =>
        "The voice filter (WeSep) keeps only your registered voice, for the customer and "
        + "the transcript, and removes other people's.\n\n"
        + $"Your voice reaches the customer about {delayMs / 1000.0:0.#} seconds late while it's on.\n\n"
        + "If it can't keep up, your voice goes out unfiltered for those moments. Untick it to "
        + "go back to normal at once.";

    const string UploadPrompt =
        "Instead of recording now, you can use a recording of your voice that you already "
        + "have, for example a WAV or MP3 file on your Desktop.\n\n"
        + "Pick one where only you are talking, with nobody else's voice in it. It needs at "
        + "least 6 seconds of you talking; YAL ClearVoice keeps 20 seconds of it, from where "
        + "you start talking.\n\n"
        + "A copy is kept on this PC, for your Windows account only, in place of any voice "
        + "you registered before. The file you pick isn't changed.";

    const string RecordConsent =
        "YAL ClearVoice will record each call you transcribe while \"Record call audio\" "
        + "is ticked, from Start to Stop, as one audio file with both you and the customer. "
        + "Your own side is also saved in the \"Agent voice\" folder at each step: as your "
        + "microphone heard it (input), after the voice filter (audio_tse), and with noise "
        + "removed too, as the customer heard it (audio_tse_se).\n\n"
        + "Only record calls where the customer has been told the call may be recorded.\n\n"
        + "Recordings stay on this PC (\"Open recordings\" shows them). Nothing is "
        + "uploaded or sent anywhere.\n\n"
        + "Start recording calls?";

    static void OpenFolder(string dir)
    {
        try
        {
            Directory.CreateDirectory(dir);
            Process.Start("explorer.exe", $"\"{dir}\"");
        }
        catch (Exception) { }
    }

    void StartEngine()
    {
        var exe = Path.Combine(AppContext.BaseDirectory, EngineName + ".exe");
        if (!File.Exists(exe)) return; // the status tile explains
        try
        {
            _engine = Process.Start(new ProcessStartInfo(exe, "--headless")
            {
                CreateNoWindow = true,
                UseShellExecute = false,
                WorkingDirectory = AppContext.BaseDirectory,
            });
        }
        catch (Exception)
        {
            _engine = null;
        }
    }

    // Stops every engine, including one left behind by a window that crashed (ours
    // then exited at once, as a second instance, and we talked to that one).
    static void StopEngines()
    {
        foreach (var p in Process.GetProcessesByName(EngineName))
        {
            try { p.Kill(); }
            catch (Exception) { }
        }
    }

    void BuildLayout()
    {
        AddHeader();
        int y = HeaderHeight + 16;
        void Header(string t)
        {
            Controls.Add(new Label { Text = t, ForeColor = Muted, AutoSize = true, Location = new Point(16, y) });
            y += 22;
        }
        void Row(string label, Control c)
        {
            Controls.Add(new Label { Text = label, AutoSize = true, Location = new Point(16, y + 4) });
            c.Location = new Point(150, y);
            c.Width = 268;
            Controls.Add(c);
            y += 34;
        }

        Header("Devices");
        Row("Microphone", _mic);
        // Where the agent hears the customer (and what the transcript listens to).
        Controls.Add(new Label { Text = "Headset", AutoSize = true, Location = new Point(16, y) });
        _headset.AutoSize = true;
        _headset.Location = new Point(150, y);
        _headset.Text = "—";
        Controls.Add(_headset);
        y += 28;
        Row("Output", _spk);

        y += 6;
        Header("Noise cancellation");
        _nc.Location = new Point(16, y);
        _nc.ForeColor = Teal;
        Controls.Add(_nc);
        _liveTse.Location = new Point(176, y);
        Controls.Add(_liveTse);
        _liveTseStatus.Location = new Point(340, y + 2);
        Controls.Add(_liveTseStatus);
        y += 36;

        Header("Status");
        var grid = new TableLayoutPanel
        {
            Location = new Point(16, y), Size = new Size(402, 56),
            ColumnCount = 3, RowCount = 1, BackColor = Panel
        };
        for (int i = 0; i < 3; i++) grid.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 100F / 3));
        grid.RowStyles.Add(new RowStyle(SizeType.Percent, 100));
        void Tile(string caption, Label value)
        {
            var cell = new FlowLayoutPanel { FlowDirection = FlowDirection.TopDown, Dock = DockStyle.Fill, Margin = new Padding(10, 8, 10, 0) };
            cell.Controls.Add(new Label { Text = caption, ForeColor = Muted, AutoSize = true, Margin = Padding.Empty });
            value.AutoSize = true;
            value.Margin = Padding.Empty;
            value.Font = new Font("Segoe UI", 10F, FontStyle.Bold);
            value.Text = "—";
            cell.Controls.Add(value);
            grid.Controls.Add(cell);
        }
        Tile("Connection", _conn);
        Tile("Noise removal (AI)", _dfn);
        Tile("Latency", _latency);
        y += grid.Height + 12; // read before Add: adding a control DPI-scales it
        Controls.Add(grid);

        // What to do next: setup tips, or how to fix the current notice. The messages
        // carry their own line breaks: a fixed-width wrapping label gets DPI-scaled
        // twice on .NET Framework (measured: 402 px became 628 at 125%).
        _hint.AutoSize = true;
        _hint.Location = new Point(16, y);
        Controls.Add(_hint);
        y += 68; // the hint's three lines at most

        // Summary notes: saved by themselves, to a folder the agent (or IT) picks.
        Header("Summary notes are saved to");
        // Framed: a fixed-size label gets DPI-scaled twice and would run into
        // the right column.
        Framed(_notesPath, Bg, 16, y, 402, 20);
        y += 22;
        _notesChange.Location = new Point(16, y);
        _notesChange.Size = new Size(90, 26);
        Controls.Add(_notesChange);
        _notesOpen.Location = new Point(112, y);
        _notesOpen.Size = new Size(100, 26);
        Controls.Add(_notesOpen);
        _noteStatus.Location = new Point(222, y + 5);
        Controls.Add(_noteStatus);
        y += 40;

        // Recording calls, with the agent's consent.
        Header("Call recording");
        _record.Location = new Point(16, y);
        Controls.Add(_record);
        _recStatus.Location = new Point(160, y + 2);
        Controls.Add(_recStatus);
        y += 28;
        _recOpen.Location = new Point(16, y);
        _recOpen.Size = new Size(120, 26);
        Controls.Add(_recOpen);
        _voiceRec.Location = new Point(142, y);
        _voiceRec.Size = new Size(128, 26);
        Controls.Add(_voiceRec);
        _voiceFile.Location = new Point(276, y);
        _voiceFile.Size = new Size(110, 26);
        Controls.Add(_voiceFile);
        y += 32;
        _voiceStatus.Location = new Point(142, y + 5);
        Controls.Add(_voiceStatus);

        // Right column: the call. Nirmala UI (part of Windows) has Latin and the
        // Indian scripts; Segoe UI in a RichTextBox shows Hindi as empty boxes.
        const int x = 456, w = 448, t = HeaderHeight;
        Controls.Add(new Label { Text = "Live transcript", ForeColor = Muted, AutoSize = true, Location = new Point(x, t + 16) });
        _callState.Location = new Point(x + 110, t + 16);
        Controls.Add(_callState);
        _transcript.Font = new Font("Nirmala UI", 9.5F);
        Framed(_transcript, Panel, x, t + 38, w, 262);
        _live.Font = new Font("Nirmala UI", 9F, FontStyle.Italic);
        Framed(_live, Bg, x, t + 302, w, 38);
        _transcribe.Location = new Point(x, t + 344);
        _transcribe.Size = new Size(170, 30);
        _transcribe.FlatStyle = FlatStyle.Flat;
        _transcribe.BackColor = Teal;
        _transcribe.ForeColor = Color.White;
        Controls.Add(_transcribe);
        _history.Location = new Point(x + 180, t + 344);
        _history.Size = new Size(130, 30);
        Controls.Add(_history);
        _save.Location = new Point(x + 324, t + 350);
        Controls.Add(_save);

        Controls.Add(new Label { Text = "After-call summary (written when you stop)", ForeColor = Muted, AutoSize = true, Location = new Point(x, t + 392) });
        _copy.Location = new Point(x + w - 70, t + 386);
        _copy.Size = new Size(70, 26);
        Controls.Add(_copy);
        Framed(_summary, Panel, x, t + 416, w, 128);
    }

    // The YAL logo and the product name across the top of the window.
    const int HeaderHeight = 44;
    void AddHeader()
    {
        Controls.Add(new PictureBox
        {
            Image = Brand.LogoImage(), SizeMode = PictureBoxSizeMode.Zoom,
            Location = new Point(16, 14), Size = new Size(70, 26),
        });
        Controls.Add(new Label
        {
            Text = "ClearVoice", AutoSize = true, ForeColor = Ink,
            Font = new Font("Segoe UI Semibold", 13F), Location = new Point(90, 14),
        });
    }

    // Places `inner` filling a box at (x, y). Sized directly, multiline boxes come
    // out DPI-scaled twice in height (measured: 262 px became 410 at 125%); docked
    // in a panel they follow the panel, which scales once.
    void Framed(Control inner, Color back, int x, int y, int w, int h)
    {
        var frame = new System.Windows.Forms.Panel
        {
            Location = new Point(x, y), Size = new Size(w, h), BackColor = back,
            Padding = back == Bg ? Padding.Empty : new Padding(8, 6, 2, 6),
        };
        inner.Dock = DockStyle.Fill;
        frame.Controls.Add(inner);
        Controls.Add(frame);
    }

    void OnDevicePicked(ComboBox box, string kind)
    {
        if (_loading || box.SelectedItem is not DeviceItem d) return;
        _commands.Enqueue(d.Id is null ? $"{kind} default" : $"{kind} {d.Id}");
    }

    // ---- pipe worker (background thread) ------------------------------------

    void Worker()
    {
        while (_running)
        {
            try
            {
                using var pipe = new NamedPipeClientStream(".", "YalClearVoice", PipeDirection.InOut);
                pipe.Connect(1000);
                using var reader = new StreamReader(pipe);
                using var writer = new StreamWriter(pipe) { AutoFlush = true, NewLine = "\n" };

                for (int polls = 0; _running && pipe.IsConnected; polls++)
                {
                    // The device list now and every ~5 s, so a headset plugged in
                    // later shows up.
                    if (polls % 20 == 0)
                    {
                        writer.WriteLine("list");
                        ApplyDevices(reader.ReadLine());
                    }
                    while (_commands.TryDequeue(out var cmd))
                    {
                        writer.WriteLine(cmd);
                        reader.ReadLine(); // telemetry reply, ignored; the poll below refreshes
                    }
                    writer.WriteLine("poll");
                    ApplyTelemetry(reader.ReadLine());
                    writer.WriteLine($"transcript {_callId} {_lines.Count}");
                    ApplyTranscript(reader.ReadLine());
                    Thread.Sleep(250);
                }
            }
            catch (Exception)
            {
                SetDisconnected();
                Thread.Sleep(1000);
            }
        }
    }

    // One engine reply as a JSON object, or null if it isn't of the expected type.
    static Dictionary<string, object>? Parse(string? line, string type) =>
        line is not null && Json.DeserializeObject(line) is Dictionary<string, object> d
        && d.TryGetValue("type", out var t) && type.Equals(t)
            ? d : null;

    // Fills the pickers when the device list changes, keeping each choice while
    // its device is still there.
    void ApplyDevices(string? line)
    {
        if (Parse(line, "devices") is not { } root) return;
        var key = new StringBuilder();
        List<DeviceItem> Items(string listKey, bool microphones)
        {
            var items = new List<DeviceItem> { new(null, "Automatic") };
            foreach (Dictionary<string, object> d in (IEnumerable)root[listKey])
            {
                var name = d["name"] as string ?? "?";
                // Never offer a virtual device (VB-CABLE, SoliCall) as the
                // microphone: the engine won't record it.
                if (microphones && IsVirtual(name)) continue;
                items.Add(new DeviceItem(d["id"] as string, name));
                key.Append(d["id"]).Append('|');
            }
            return items;
        }
        var mics = Items("inputs", microphones: true);
        var spks = Items("outputs", microphones: false);
        if (key.ToString() == _deviceKey) return;
        _deviceKey = key.ToString();
        RunOnUi(() =>
        {
            _loading = true;
            // A picked microphone that's gone: back to automatic, in the engine too.
            if (!Fill(_mic, mics)) _commands.Enqueue("mic default");
            if (!_spkLocked) Fill(_spk, spks);
            _loading = false;
        });
    }

    // Replaces a picker's items, keeping the selection if its device is still
    // listed; false if it had to fall back to Automatic.
    static bool Fill(ComboBox box, List<DeviceItem> items)
    {
        var keep = (box.SelectedItem as DeviceItem)?.Id;
        box.Items.Clear();
        box.Items.AddRange(items.ToArray());
        int i = items.FindIndex(d => d.Id == keep);
        box.SelectedIndex = Math.Max(i, 0);
        return i >= 0;
    }

    static bool IsVirtual(string name) =>
        name.Contains("VB-Audio") || name.Contains("CABLE") || name.Contains("SoliCall")
        || name.Contains("YAL ClearVoice");

    // "Microphone (Detecta 201 USB)" -> "Detecta 201 USB": Windows wraps the
    // device's own name in the kind of endpoint.
    static string Short(string name)
    {
        int open = name.IndexOf('(');
        return open > 0 && name.EndsWith(")") ? name.Substring(open + 1, name.Length - open - 2) : name;
    }

    void ApplyTelemetry(string? line)
    {
        if (Parse(line, "telemetry") is not { } r) return;
        bool mic = (bool)r["mic_connected"];
        bool spk = (bool)r["spk_connected"];
        bool spkSelectable = !r.TryGetValue("spk_selectable", out var sel) || (bool)sel;
        bool nc = (bool)r["nc"];
        string dfn = r["dfn"] as string ?? "—";
        int notice = r.TryGetValue("notice", out var n) ? Convert.ToInt32(n) : 0;
        long latency = Convert.ToInt64(r["latency_ms"]);
        string micName = r.TryGetValue("mic_name", out var mn) ? mn as string ?? "" : "";
        string headsetName = r.TryGetValue("headset_name", out var hn) ? hn as string ?? "" : "";
        string voice = r.TryGetValue("voice", out var vo) ? vo as string ?? "off" : "off";
        string liveTse = r.TryGetValue("live_tse", out var lt) ? lt as string ?? "unavailable" : "unavailable";
        var (liveText, liveColor) = liveTse switch
        {
            "on" => ("On", Teal),
            "late" => ("PC too slow", Ink),
            "failed" => ("Error", Ink),
            "server_down" => ("Server not reachable", Ink),
            _ => ("", Muted),
        };
        long liveDelay = r.TryGetValue("live_tse_delay_ms", out var ld) ? Convert.ToInt64(ld) : 0;
        var (voiceText, voiceColor) = voice switch
        {
            "no_sample" => ("Not registered yet", Ink),
            "recording" => ("● Speak now, for 20 seconds", Teal),
            "ready" => ("Voice registered", Muted),
            "server_down" => ("Voice server not reachable", Ink),
            "too_quiet" => ("Didn't hear you: try again", Ink),
            "failed" => ("Couldn't record: try again", Ink),
            "unavailable" => ("Voice filter couldn't start", Ink),
            "importing" => ("Reading the file...", Teal),
            "bad_file" => ("Couldn't read that file: try another", Ink),
            "short_file" => ("Under 6 s of talking in that file: try another", Ink),
            _ => ("", Muted),
        };
        // The AI that removes background noise from the agent's voice. Whenever it
        // isn't active, the voice still reaches the call, just not cleaned.
        string dfnText = !nc ? "Off" : dfn switch
        {
            "running" => "Active",
            "overloaded" => "Paused (PC busy)",
            "failed" => "Stopped (error)",
            _ => "Starting…",
        };

        var (connText, connColor) = notice switch
        {
            1 => ("No VB-CABLE", Ink),
            2 => ("No model", Ink),
            3 => ("Check speaker", Ink),
            _ when mic && spk => ("Connected", Teal),
            _ when mic => ("Mic only", Muted),
            _ when spk => ("Output only", Muted),
            _ => ("Waiting…", Muted),
        };
        string? hint = notice switch
        {
            1 => "VB-CABLE isn't installed. Install it from\n"
               + "vb-audio.com/Cable, restart the PC, then open\nYAL ClearVoice again.",
            2 => "The model file DeepFilterNet3_onnx.tar.gz is missing.\n"
               + "Keep it in the same folder as this app.",
            3 => "Your PC's sound is going to VB-CABLE, so you can't hear\n"
               + "anything. Open Settings > System > Sound and set\nOutput to your headset.",
            _ when !spkSelectable => "In Zoom / Teams / Meet, choose \"CABLE Output\" as your\n"
               + "microphone and your headset as the speaker. Keep this\nwindow open during calls.",
            _ => null,
        };

        RunOnUi(() =>
        {
            _loading = true;
            _conn.Text = connText;
            _conn.ForeColor = connColor;
            _dfn.Text = dfnText;
            _dfn.ForeColor = nc && dfn == "running" ? Teal : Muted;
            // Name the devices in use: "Automatic (Detecta 201 USB)".
            var auto = mic && micName.Length > 0 ? $"Automatic ({Short(micName)})" : "Automatic";
            if (_mic.Items.Count > 0 && _mic.Items[0].ToString() != auto)
                _mic.Items[0] = new DeviceItem(null, auto);
            _headset.Text = headsetName.Length > 0 ? Short(headsetName) : "None found: plug in your headset";
            _headset.ForeColor = headsetName.Length > 0 ? Ink : Muted;
            _latency.Text = latency > 0 ? $"{latency} ms" : "—";
            _nc.Checked = nc;
            _liveTse.Visible = liveTse != "unavailable";
            if (liveDelay > 0) _liveTseDelayMs = liveDelay;
            if (_liveTseWanted is null && liveTse != "unavailable")
            {
                _liveTseWanted = liveTse != "off";
                _liveTse.Checked = _liveTseWanted.Value;
            }
            bool engineOn = liveTse != "off" && liveTse != "unavailable";
            if (_liveTseWanted is bool want && liveTse != "unavailable" && want != engineOn)
                _commands.Enqueue(want ? "tse on" : "tse off");
            _liveTseStatus.Text = _liveTseWanted == true && liveTse == "off" ? "Starting…" : liveText;
            _liveTseStatus.ForeColor = liveColor;
            if (!spkSelectable && !_spkLocked)
            {
                _spk.Items.Clear();
                _spk.Items.Add(new DeviceItem(null, "VB-CABLE → your call app"));
                _spk.SelectedIndex = 0;
                _spk.Enabled = false;
                _spkLocked = true;
            }
            ShowHint(hint, notice != 0);
            _voiceRec.Visible = _voiceFile.Visible = _voiceStatus.Visible = voice != "off";
            _voiceRec.Enabled = _voiceFile.Enabled = voice != "recording" && voice != "importing";
            _voiceStatus.Text = voiceText;
            _voiceStatus.ForeColor = voiceColor;
            _loading = false;
        });
    }

    // New transcript lines (or a whole new call), what's being said now, and the
    // call's state. A call that has ended gets summarized once.
    void ApplyTranscript(string? line)
    {
        if (Parse(line, "transcript") is not { } t) return;
        bool enabled = (bool)t["enabled"];
        int call = Convert.ToInt32(t["call"]);
        bool active = (bool)t["active"];
        string error = t["error"] as string ?? "";
        bool newCall = call != _callId;
        if (newCall)
        {
            _callId = call;
            _callStart = DateTime.Now;
            _lines.Clear();
        }
        var added = new List<KeyValuePair<bool, string>>();
        foreach (Dictionary<string, object> l in (IEnumerable)t["lines"])
        {
            bool customer = "customer".Equals(l["who"]);
            string text = l["text"] as string ?? "";
            added.Add(new(customer, text));
            _lines.Add((customer ? "Customer: " : "Agent: ") + text);
        }
        var live = (Dictionary<string, object>)t["live"];
        string Live(string who, string label) =>
            live[who] is string s && s.Length > 0
                ? label + (s.Length > 150 ? "…" + s.Substring(s.Length - 150) : s) + "\n" : "";
        string liveText = Live("customer", "Customer: ") + Live("agent", "You: ");

        if (!active && _lines.Count > 0 && !_summarizing && _summarizedCall != call)
        {
            _summarizedCall = call;
            StartSummary(call, new List<string>(_lines), _callStart, _saveCalls);
        }

        // After a recorded call, the engine's after-call pass may send that
        // call's improved transcript, minutes later: it replaces the call's
        // note and saved copy, and the window's view if it still shows it.
        if (t.TryGetValue("improved", out var imp) && imp is Dictionary<string, object> improved)
        {
            int which = Convert.ToInt32(improved["call"]);
            if (which != _improvedCall && !_summarizing && _ended.TryGetValue(which, out var when))
            {
                _improvedCall = which;
                var better = new List<string>();
                var shown = new List<KeyValuePair<bool, string>>();
                foreach (Dictionary<string, object> l in (IEnumerable)improved["lines"])
                {
                    bool customer = "customer".Equals(l["who"]);
                    string text = l["text"] as string ?? "";
                    better.Add((customer ? "Customer: " : "Agent: ") + text);
                    shown.Add(new(customer, text));
                }
                if (better.Count > 0)
                {
                    if (which == call && !active)
                    {
                        _lines.Clear();
                        _lines.AddRange(better);
                        RunOnUi(() =>
                        {
                            _transcript.Clear();
                            _transcript.SelectionColor = Muted;
                            _transcript.AppendText("Improved after the call (voice filter on the whole call):\n");
                            foreach (var a in shown) AppendLine(a.Key, a.Value);
                        });
                    }
                    StartSummary(which, better, when.Start, _saveCalls, when.End);
                }
            }
        }

        // A recording follows the transcribed call while wanted: one file per call.
        bool recStarted = false;
        if (_recordWanted && active && _recordingCall != call)
        {
            var file = Path.Combine(Recordings, DateTime.Now.ToString("yyyy-MM-dd HH-mm-ss", CultureInfo.InvariantCulture) + ".wav");
            _commands.Enqueue("rec start " + file);
            _recordingCall = call;
            recStarted = true;
        }
        else if ((!_recordWanted || !active) && _recordingCall != 0)
        {
            _commands.Enqueue("rec stop");
            _recordingCall = 0;
        }
        bool recording = _recordingCall != 0;

        var (stateText, stateColor) =
            !enabled ? ("off: no Sarvam key in settings.ini", Muted)
            : error.Length > 0 ? ("problem (see below)", Ink)
            : active ? ("● transcribing", Teal)
            : ("stopped", Muted);
        RunOnUi(() =>
        {
            if (newCall) _transcript.Clear();
            foreach (var a in added) AppendLine(a.Key, a.Value);
            _live.Text = error.Length > 0 ? error : liveText;
            _live.ForeColor = error.Length > 0 ? Ink : Muted;
            _callState.Text = stateText;
            _callState.ForeColor = stateColor;
            _transcribing = active;
            _transcribe.Text = active ? "■  Stop transcription" : "▶  Start transcription";
            _transcribe.BackColor = active ? Ink : Teal;
            _transcribe.Enabled = enabled;
            if (recStarted)
            {
                _recStatus.Text = "● Recording this call";
                _recStatus.ForeColor = Teal;
            }
            else if (!recording && _recStatus.ForeColor == Teal)
            {
                _recStatus.Text = _recordWanted ? "On: records each transcribed call" : "";
                _recStatus.ForeColor = Muted;
            }
        });
    }

    void AppendLine(bool customer, string text)
    {
        _transcript.SelectionStart = _transcript.TextLength;
        _transcript.SelectionLength = 0;
        _bold ??= new Font(_transcript.Font, FontStyle.Bold);
        _transcript.SelectionFont = _bold;
        _transcript.SelectionColor = customer ? Teal : Muted;
        _transcript.AppendText(customer ? "Customer  " : "You  ");
        _transcript.SelectionFont = _transcript.Font;
        _transcript.SelectionColor = customer ? Ink : Muted;
        _transcript.AppendText(text + "\n");
        _transcript.ScrollToCaret();
    }

    // ---- after-call summary (background thread) ----------------------------

    const string SummaryPrompt =
        "You write after-call notes for a call-center agent. You get the transcript of one phone "
        + "call between the agent and a customer. It comes from automatic speech recognition, so "
        + "some words may be wrong, and it may mix English with Hindi or other Indian languages.\n\n"
        + "Write the notes in English, as plain text (no markdown, no asterisks), under exactly "
        + "these headings:\n"
        + "Reason for call:\n"
        + "Key details: (names, account, order or reference numbers, dates and amounts that were "
        + "said; \"None\" if none)\n"
        + "What was done:\n"
        + "Follow-up: (what the agent or the customer still has to do, with any deadline; \"None\" "
        + "if nothing)\n"
        + "Customer mood:\n\n"
        + "One or two short lines per heading. Use only what is in the transcript; never invent details.";

    // The server's context is ~10k tokens: a very long call keeps its start and end.
    const int MaxTranscriptChars = 16000;

    // Summarizes finished call `call` (its `lines`, begun at `start`). A summary
    // that came is saved as a note in the summary notes folder. With `save`, the
    // call is also kept for Previous calls: at once with the transcript, so it's
    // kept even if the summary never comes, then again with the summary.
    // `improvedEnd` marks a second summary, from the after-call pass's improved
    // transcript: it keeps the call's times, replaces its note and saved copy
    // only when it came, and shows only if the window still shows that call.
    void StartSummary(int call, List<string> lines, DateTime start, bool save, DateTime? improvedEnd = null)
    {
        _summarizing = true;
        bool improved = improvedEnd is not null;
        var end = improvedEnd ?? DateTime.Now;
        if (!improved)
        {
            _ended[call] = (start, end);
            if (save) CallStore.Save(start, end, "(not written yet)", lines);
            RunOnUi(() => _summary.Text = "Writing the summary…");
        }
        new Thread(() =>
        {
            string text;
            bool ok;
            try { text = Summarize(string.Join("\n", lines), out ok); }
            finally { _summarizing = false; }
            if (improved && !ok) return; // the first summary stays
            string? note = null;
            if (ok)
            {
                try
                {
                    Notes.Save(start, end, text);
                    note = improved
                        ? $"Note improved at {DateTime.Now:HH:mm}"
                        : $"Note saved at {DateTime.Now:HH:mm}";
                }
                catch (Exception e)
                {
                    note = "Couldn't save the note: " + (e.Message.Length > 60 ? e.Message.Substring(0, 60) + "…" : e.Message);
                }
            }
            if (save && CallStore.Save(start, end, text, lines) is { } error)
                text += $"\r\n\r\n(This call couldn't be saved: {error})";
            RunOnUi(() =>
            {
                if (improved && call != _callId) return;
                _summary.Text = improved ? "Improved after the call:\r\n" + text : text;
                if (note is not null)
                {
                    _noteStatus.Text = note;
                    _noteStatus.ForeColor = note.StartsWith("Note ") ? Teal : Ink;
                }
            });
        }) { IsBackground = true }.Start();
    }

    // The summary for `transcript`; `ok` is false when the text is instead an
    // error to show (no server set up, refused, unreachable).
    static string Summarize(string transcript, out bool ok)
    {
        ok = false;
        var s = ReadSettings("summary");
        if (!s.TryGetValue("url", out var url) || !s.TryGetValue("model", out var model))
            return "No summary server is set up: settings.ini needs a [summary] section with url and model.";
        if (transcript.Length > MaxTranscriptChars)
            transcript = transcript.Substring(0, 4000) + "\n[… middle of the call left out …]\n"
                + transcript.Substring(transcript.Length - (MaxTranscriptChars - 4000));
        var json = new JavaScriptSerializer();
        var body = Encoding.UTF8.GetBytes(json.Serialize(new Dictionary<string, object>
        {
            ["model"] = model,
            ["temperature"] = 0.2,
            ["max_tokens"] = 700,
            ["messages"] = new object[]
            {
                new Dictionary<string, object> { ["role"] = "system", ["content"] = SummaryPrompt },
                new Dictionary<string, object> { ["role"] = "user", ["content"] = "Transcript:\n" + transcript },
            },
        }));
        try
        {
            var request = (HttpWebRequest)WebRequest.Create(url);
            request.Method = "POST";
            request.ContentType = "application/json";
            request.Timeout = 90000;
            if (s.TryGetValue("api_key", out var key) && key.Length > 0)
                request.Headers[HttpRequestHeader.Authorization] = "Bearer " + key;
            using (var stream = request.GetRequestStream()) stream.Write(body, 0, body.Length);
            using var response = request.GetResponse();
            using var reader = new StreamReader(response.GetResponseStream(), Encoding.UTF8);
            var reply = (Dictionary<string, object>)json.DeserializeObject(reader.ReadToEnd());
            var choice = (Dictionary<string, object>)((object[])reply["choices"])[0];
            var content = ((Dictionary<string, object>)choice["message"])["content"] as string ?? "";
            // TextBoxes and Notepad want Windows line breaks.
            ok = content.Trim().Length > 0;
            return content.Replace("**", "").Trim().Replace("\r\n", "\n").Replace("\n", "\r\n");
        }
        catch (WebException e) when (e.Response is HttpWebResponse r)
        {
            return $"The summary server refused the request (HTTP {(int)r.StatusCode}). "
                + "Check [summary] in settings.ini. The transcript above can still be copied.";
        }
        catch (Exception e)
        {
            return $"Couldn't reach the summary server ({e.Message}). "
                + "The transcript above can still be copied.";
        }
    }

    // One [section] of settings.ini (next to this app) as key = value pairs.
    internal static Dictionary<string, string> ReadSettings(string section)
    {
        var result = new Dictionary<string, string>();
        var path = Path.Combine(AppContext.BaseDirectory, "settings.ini");
        if (!File.Exists(path)) return result;
        string current = "";
        foreach (var raw in File.ReadAllLines(path))
        {
            var line = raw.Trim();
            int eq = line.IndexOf('=');
            if (line.StartsWith("[") && line.EndsWith("]")) current = line.Substring(1, line.Length - 2).Trim();
            else if (current == section && !line.StartsWith(";") && eq > 0)
                result[line.Substring(0, eq).Trim()] = line.Substring(eq + 1).Trim();
        }
        return result;
    }

    void SetDisconnected() => RunOnUi(() =>
    {
        bool missing = _engine is null;
        bool stopped = _engine is { HasExited: true };
        _conn.Text = missing ? "Engine missing" : stopped ? "Engine stopped" : "Starting…";
        _conn.ForeColor = Muted;
        foreach (var l in new[] { _dfn, _latency, _headset }) l.Text = "—";
        _transcribe.Enabled = false;
        ShowHint(
            missing ? $"\"{EngineName}.exe\" must be in the same folder\nas this app."
            : stopped ? "The engine stopped. Close this window and open\nYAL ClearVoice again."
            : null,
            problem: true);
    });

    void ShowHint(string? text, bool problem)
    {
        _hint.Text = text ?? "";
        _hint.ForeColor = problem ? Ink : Muted;
        _hint.Visible = text is not null;
    }

    void RunOnUi(Action a)
    {
        if (IsDisposed || !IsHandleCreated) return;
        try { BeginInvoke(a); }
        catch (ObjectDisposedException) { }
        catch (InvalidOperationException) { }
    }
}

/// Summary notes: each call's summary, saved by itself as a text file in a
/// designated folder (for the agent's records, or a shared folder for a team).
/// The folder is the agent's choice ("Change…", remembered per Windows user),
/// else settings.ini's [notes] folder (set by IT; may be a network folder),
/// else Documents\YAL ClearVoice\Summary notes.
static class Notes
{
    const string Key = @"Software\YAL ClearVoice";

    public static string Folder
    {
        get
        {
            using (var k = Registry.CurrentUser.OpenSubKey(Key))
                if (k?.GetValue("SummaryFolder") is string chosen && chosen.Length > 0) return chosen;
            if (MainForm.ReadSettings("notes").TryGetValue("folder", out var designated) && designated.Length > 0)
                return Environment.ExpandEnvironmentVariables(designated);
            return Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments),
                "YAL ClearVoice", "Summary notes");
        }
        set
        {
            using var k = Registry.CurrentUser.CreateSubKey(Key);
            k.SetValue("SummaryFolder", value);
        }
    }

    // Saves the summary of the call from `start` to `end`; returns the file's path.
    // Named by start time and agent, so a shared folder never mixes up two agents.
    public static string Save(DateTime start, DateTime end, string summary)
    {
        var dir = Folder;
        Directory.CreateDirectory(dir);
        var agent = string.Concat(Environment.UserName.Split(Path.GetInvalidFileNameChars()));
        var path = Path.Combine(dir, $"{start.ToString("yyyy-MM-dd HH-mm-ss", CultureInfo.InvariantCulture)} {agent}.txt");
        File.WriteAllText(path,
            $"YAL ClearVoice summary note\r\n"
            + $"Call: {start:d MMM yyyy}, {start:HH:mm} to {end:HH:mm}\r\n"
            + $"Agent: {Environment.UserName} on {Environment.MachineName}\r\n\r\n"
            + summary + "\r\n",
            new UTF8Encoding(true));
        return path;
    }
}

/// Finished calls saved on this PC, one text file each (summary, then transcript;
/// Notepad opens them too), named by start time, in %LOCALAPPDATA%\YAL
/// ClearVoice\Transcripts: private to the Windows user and not synced by
/// OneDrive, unlike Desktop or Documents.
static class CallStore
{
    public static readonly string Folder = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
        "YAL ClearVoice", "Transcripts");
    const string NameFormat = "yyyy-MM-dd HH-mm-ss";
    const string Key = @"Software\YAL ClearVoice";

    // "Save calls", remembered for this Windows user; on unless switched off.
    public static bool Enabled
    {
        get
        {
            using var k = Registry.CurrentUser.OpenSubKey(Key);
            return k?.GetValue("SaveCalls") is not int v || v != 0;
        }
        set
        {
            using var k = Registry.CurrentUser.CreateSubKey(Key);
            k.SetValue("SaveCalls", value ? 1 : 0, RegistryValueKind.DWord);
        }
    }

    // Writes (or rewrites) the call that started at `start`; null, or what went wrong.
    public static string? Save(DateTime start, DateTime end, string summary, List<string> lines)
    {
        try
        {
            Directory.CreateDirectory(Folder);
            var text = $"YAL ClearVoice call, {start:d MMM yyyy}, {start:HH:mm} to {end:HH:mm}\r\n\r\n"
                + $"SUMMARY\r\n{summary}\r\n\r\nTRANSCRIPT\r\n{string.Join("\r\n", lines)}\r\n";
            File.WriteAllText(Path.Combine(Folder, start.ToString(NameFormat, CultureInfo.InvariantCulture) + ".txt"),
                text, new UTF8Encoding(true));
            return null;
        }
        catch (Exception e)
        {
            return e.Message;
        }
    }

    // Changes whenever a call file is added, removed or rewritten.
    public static string Stamp() => !Directory.Exists(Folder) ? ""
        : string.Join("|", Directory.GetFiles(Folder, "*.txt").Select(f => f + File.GetLastWriteTimeUtc(f).Ticks));

    // Every saved call, newest first.
    public static List<SavedCall> Load()
    {
        var calls = new List<SavedCall>();
        if (!Directory.Exists(Folder)) return calls;
        foreach (var file in Directory.GetFiles(Folder, "*.txt"))
        {
            if (!DateTime.TryParseExact(Path.GetFileNameWithoutExtension(file), NameFormat,
                    CultureInfo.InvariantCulture, DateTimeStyles.None, out var when)) continue;
            string text;
            try { text = File.ReadAllText(file); }
            catch (IOException) { continue; }
            // Listed as its time and the customer's first words.
            var first = text.Split('\n').Select(l => l.Trim()).FirstOrDefault(l => l.StartsWith("Customer: "));
            var preview = first?.Substring("Customer: ".Length) ?? "(no customer speech)";
            if (preview.Length > 45) preview = preview.Substring(0, 45) + "…";
            calls.Add(new SavedCall(when, $"{when:d MMM, HH:mm}   {preview}", text));
        }
        calls.Sort((a, b) => b.When.CompareTo(a.When));
        return calls;
    }
}

sealed class SavedCall
{
    public SavedCall(DateTime when, string title, string text)
    {
        When = when;
        Title = title;
        Text = text;
    }
    public DateTime When { get; }
    public string Title { get; }
    public string Text { get; }
    public override string ToString() => Title;
}

/// "Previous calls": the saved calls, newest first, searchable (any word in the
/// summary or transcript, e.g. an order number); the selected one in full, with
/// Copy and Open folder. Brought to the front, it re-reads the folder if it changed.
sealed class HistoryForm : Form
{
    readonly TextBox _search = new() { Width = 280, Margin = new Padding(0, 3, 12, 0) };
    readonly ListBox _list = new() { IntegralHeight = false, BorderStyle = BorderStyle.None };
    readonly TextBox _view = new()
    {
        Multiline = true, ReadOnly = true, ScrollBars = ScrollBars.Vertical, BorderStyle = BorderStyle.None,
    };
    List<SavedCall> _all = new();

    public HistoryForm(Color back, Color panel)
    {
        SuspendLayout();
        AutoScaleDimensions = new SizeF(96F, 96F);
        AutoScaleMode = AutoScaleMode.Dpi;
        Text = "YAL ClearVoice: previous calls";
        Icon = Brand.AppIcon();
        ClientSize = new Size(880, 540);
        BackColor = back;
        Font = new Font("Segoe UI", 9F);
        StartPosition = FormStartPosition.CenterParent;

        var copy = new Button { Text = "Copy", Size = new Size(80, 26) };
        var folder = new Button { Text = "Open folder", Size = new Size(100, 26) };
        var top = new FlowLayoutPanel { Dock = DockStyle.Top, Height = 42, Padding = new Padding(10, 8, 10, 0) };
        top.Controls.Add(new Label { Text = "Search", AutoSize = true, Margin = new Padding(0, 6, 6, 0) });
        top.Controls.Add(_search);
        top.Controls.Add(copy);
        top.Controls.Add(folder);
        // Nirmala UI shows Hindi and the other Indian scripts as well as English.
        _list.Font = _view.Font = new Font("Nirmala UI", 9.5F);
        _list.BackColor = _view.BackColor = panel;
        _list.Dock = DockStyle.Left;
        _list.Width = 320;
        _view.Dock = DockStyle.Fill;
        // Docking runs last-added first: the top bar, the list, a gap, then the view.
        Controls.Add(_view);
        Controls.Add(new System.Windows.Forms.Panel { Dock = DockStyle.Left, Width = 10 });
        Controls.Add(_list);
        Controls.Add(top);
        ResumeLayout();

        _search.TextChanged += (_, _) => Filter();
        _list.SelectedIndexChanged += (_, _) =>
        {
            if (_list.SelectedItem is SavedCall c) _view.Text = c.Text;
        };
        copy.Click += (_, _) =>
        {
            if (_view.Text.Length > 0) Clipboard.SetText(_view.Text);
        };
        folder.Click += (_, _) =>
        {
            Directory.CreateDirectory(CallStore.Folder);
            Process.Start("explorer.exe", $"\"{CallStore.Folder}\"");
        };
        Activated += (_, _) =>
        {
            var stamp = CallStore.Stamp();
            if (stamp == _stamp) return;
            _stamp = stamp;
            _all = CallStore.Load();
            Filter();
        };
    }
    string? _stamp; // the folder as last read

    // Lists the calls containing the search text, keeping the selected one.
    void Filter()
    {
        var keep = (_list.SelectedItem as SavedCall)?.When;
        var q = _search.Text.Trim();
        var shown = _all.Where(c => q.Length == 0 || c.Text.IndexOf(q, StringComparison.OrdinalIgnoreCase) >= 0).ToList();
        _list.BeginUpdate();
        _list.Items.Clear();
        _list.Items.AddRange(shown.ToArray());
        _list.EndUpdate();
        if (shown.Count > 0)
            _list.SelectedIndex = Math.Max(shown.FindIndex(c => c.When == keep), 0);
        else
            _view.Text = _all.Count == 0
                ? "No saved calls yet. A call is saved here when you stop transcription, while \"Save calls\" is ticked."
                : "No saved call contains \"" + q + "\".";
    }
}
