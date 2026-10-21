// yalcv-devnode.exe - creates/removes the YAL ClearVoice root devnode and binds the
// driver, for the installer. There is no inbox CLI that creates a root-enumerated
// software device (devgen/devcon are dev tools, not redistributable), so the MSI
// ships this tiny SetupAPI helper and its custom actions call it:
//
//   yalcv-devnode.exe install   create ROOT\YALCLEARVOICE (if absent) + bind the driver
//   yalcv-devnode.exe remove    delete the devnode
//
// install finds yalcv.inf in a driver\ folder next to this exe, so the custom
// action needs no arguments or path plumbing. Exit code 0 = success.
//
// Build (x64): cl /EHsc /W4 /DUNICODE /D_UNICODE devnode.cpp setupapi.lib newdev.lib
// Sign the result; the MSI runs it elevated.

#include <windows.h>
#include <newdev.h>
#include <setupapi.h>
#include <cstdio>
#include <cwchar>

// MEDIA device setup class; matches yalcv.inf.
static const GUID GUID_DEVCLASS_MEDIA = {
    0x4d36e96c, 0xe325, 0x11ce, {0xbf, 0xc1, 0x08, 0x00, 0x2b, 0xe1, 0x03, 0x18}};
static const wchar_t *const HWID = L"ROOT\\YALCLEARVOICE";

// True if a present device already advertises our hardware ID.
static bool deviceExists()
{
    HDEVINFO set = SetupDiGetClassDevsW(&GUID_DEVCLASS_MEDIA, nullptr, nullptr, DIGCF_PRESENT);
    if (set == INVALID_HANDLE_VALUE)
        return false;
    bool found = false;
    SP_DEVINFO_DATA dev = {sizeof(dev)};
    for (DWORD i = 0; !found && SetupDiEnumDeviceInfo(set, i, &dev); i++)
    {
        wchar_t ids[512] = {0};
        if (SetupDiGetDeviceRegistryPropertyW(set, &dev, SPDRP_HARDWAREID, nullptr,
                                              reinterpret_cast<PBYTE>(ids), sizeof(ids), nullptr))
        {
            // REG_MULTI_SZ: compare each ID until the double null.
            for (const wchar_t *p = ids; *p; p += wcslen(p) + 1)
                if (_wcsicmp(p, HWID) == 0)
                {
                    found = true;
                    break;
                }
        }
    }
    SetupDiDestroyDeviceInfoList(set);
    return found;
}

static int createDevnode()
{
    HDEVINFO set = SetupDiCreateDeviceInfoList(&GUID_DEVCLASS_MEDIA, nullptr);
    if (set == INVALID_HANDLE_VALUE)
        return 1;
    int rc = 1;
    SP_DEVINFO_DATA dev = {sizeof(dev)};
    // Hardware ID as a double-null-terminated REG_MULTI_SZ (zero-init gives the 2nd null).
    wchar_t ids[64] = {0};
    wcsncpy_s(ids, HWID, _TRUNCATE);
    const DWORD idsBytes = static_cast<DWORD>((wcslen(HWID) + 2) * sizeof(wchar_t));
    if (SetupDiCreateDeviceInfoW(set, L"MEDIA", &GUID_DEVCLASS_MEDIA, nullptr, nullptr,
                                 DICD_GENERATE_ID, &dev) &&
        SetupDiSetDeviceRegistryPropertyW(set, &dev, SPDRP_HARDWAREID,
                                          reinterpret_cast<const BYTE *>(ids), idsBytes) &&
        SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, &dev))
    {
        rc = 0;
    }
    if (rc != 0)
        fwprintf(stderr, L"create failed: 0x%08lX\n", GetLastError());
    SetupDiDestroyDeviceInfoList(set);
    return rc;
}

// Create the devnode if absent, then stage and bind yalcv.inf (found in driver\
// next to this exe) to it.
static int installDriver()
{
    if (!deviceExists() && createDevnode() != 0)
        return 1;
    wchar_t exe[MAX_PATH] = {0};
    if (!GetModuleFileNameW(nullptr, exe, MAX_PATH))
        return 1;
    if (wchar_t *slash = wcsrchr(exe, L'\\'))
        *slash = 0;
    wchar_t inf[MAX_PATH] = {0};
    if (swprintf_s(inf, L"%s\\driver\\yalcv.inf", exe) < 0)
        return 1;
    BOOL reboot = FALSE;
    if (!UpdateDriverForPlugAndPlayDevicesW(nullptr, HWID, inf, INSTALLFLAG_FORCE, &reboot))
    {
        fwprintf(stderr, L"bind failed: 0x%08lX\n", GetLastError());
        return 1;
    }
    return 0; // a root devnode rarely needs a reboot; the MSI handles it if flagged
}

static int removeDevnode()
{
    HDEVINFO set = SetupDiGetClassDevsW(&GUID_DEVCLASS_MEDIA, nullptr, nullptr, DIGCF_PRESENT);
    if (set == INVALID_HANDLE_VALUE)
        return 0; // nothing present to remove
    int rc = 0;
    SP_DEVINFO_DATA dev = {sizeof(dev)};
    for (DWORD i = 0; SetupDiEnumDeviceInfo(set, i, &dev); i++)
    {
        wchar_t ids[512] = {0};
        if (!SetupDiGetDeviceRegistryPropertyW(set, &dev, SPDRP_HARDWAREID, nullptr,
                                               reinterpret_cast<PBYTE>(ids), sizeof(ids), nullptr))
            continue;
        bool mine = false;
        for (const wchar_t *p = ids; *p; p += wcslen(p) + 1)
            if (_wcsicmp(p, HWID) == 0)
            {
                mine = true;
                break;
            }
        if (mine && !SetupDiCallClassInstaller(DIF_REMOVE, set, &dev))
        {
            fwprintf(stderr, L"remove failed: 0x%08lX\n", GetLastError());
            rc = 1;
        }
    }
    SetupDiDestroyDeviceInfoList(set);
    return rc;
}

int wmain(int argc, wchar_t **argv)
{
    if (argc >= 2 && _wcsicmp(argv[1], L"install") == 0)
        return installDriver();
    if (argc >= 2 && _wcsicmp(argv[1], L"remove") == 0)
        return removeDevnode();
    fwprintf(stderr, L"usage: yalcv-devnode install|remove\n");
    return 2;
}
