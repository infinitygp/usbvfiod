# Walk the USB topology as Windows reports it, like USBView: for every hub port the connection
# status, speed, VID:PID and whether the device is a hub.
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

public static class UsbTree {
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern SafeFileHandle CreateFile(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DeviceIoControl(SafeFileHandle handle, uint code, byte[] input, int inputLength, byte[] output, int outputLength, out int returned, IntPtr overlapped);

    const uint IOCTL_USB_GET_NODE_INFORMATION = 0x220408;
    const uint IOCTL_USB_GET_NODE_CONNECTION_NAME = 0x220414;
    const uint IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX = 0x220448;
    static readonly string[] Speeds = { "low", "full", "high", "super" };
    static readonly string[] Status = { "NoDeviceConnected", "DeviceConnected", "DeviceFailedEnumeration", "DeviceGeneralFailure", "DeviceCausedOvercurrent", "DeviceNotEnoughPower", "DeviceNotEnoughBandwidth", "DeviceHubNestedTooDeeply", "DeviceInLegacyHub", "DeviceEnumerating", "DeviceReset" };

    static SafeFileHandle Open(string path) {
        return CreateFile(path, 0x40000000, 2, IntPtr.Zero, 3, 0, IntPtr.Zero);
    }

    public static List<string> Walk() {
        var lines = new List<string>();
        for (int i = 0; i < 8; i++) {
            using (var hcd = Open(@"\\.\HCD" + i)) {
                if (hcd.IsInvalid) continue;
                var buffer = new byte[1024];
                int returned;
                if (!DeviceIoControl(hcd, IOCTL_USB_GET_NODE_INFORMATION, null, 0, buffer, buffer.Length, out returned, IntPtr.Zero)) {
                    lines.Add("HCD" + i + ": no root hub name, error " + Marshal.GetLastWin32Error());
                    continue;
                }
                int length = BitConverter.ToInt32(buffer, 0);
                string rootHub = Encoding.Unicode.GetString(buffer, 4, length - 4).TrimEnd('\0');
                lines.Add("HCD" + i + " root hub " + rootHub);
                Hub(@"\\.\" + rootHub, 1, lines);
            }
        }
        return lines;
    }

    static void Hub(string path, int depth, List<string> lines) {
        string indent = new string(' ', depth * 2);
        using (var hub = Open(path)) {
            if (hub.IsInvalid) { lines.Add(indent + "cannot open " + path + ": error " + Marshal.GetLastWin32Error()); return; }
            var buffer = new byte[1024];
            int returned;
            if (!DeviceIoControl(hub, IOCTL_USB_GET_NODE_INFORMATION, buffer, buffer.Length, buffer, buffer.Length, out returned, IntPtr.Zero)) {
                lines.Add(indent + "node information failed, error " + Marshal.GetLastWin32Error());
                return;
            }
            // USB_NODE_INFORMATION: NodeType (4), then the hub descriptor; bNumberOfPorts is its third byte.
            int ports = buffer[4 + 2];
            for (int port = 1; port <= ports; port++) {
                var info = new byte[512];
                BitConverter.GetBytes(port).CopyTo(info, 0);
                if (!DeviceIoControl(hub, IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX, info, info.Length, info, info.Length, out returned, IntPtr.Zero)) {
                    lines.Add(indent + "port " + port + ": connection information failed, error " + Marshal.GetLastWin32Error());
                    continue;
                }
                int vid = BitConverter.ToUInt16(info, 4 + 8);
                int pid = BitConverter.ToUInt16(info, 4 + 10);
                int bcdUsb = BitConverter.ToUInt16(info, 4 + 2);
                int speed = info[23];
                bool isHub = info[24] != 0;
                int address = BitConverter.ToUInt16(info, 25);
                int status = BitConverter.ToInt32(info, 31);
                string statusName = status < Status.Length ? Status[status] : status.ToString();
                if (status == 0) {
                    lines.Add(indent + "port " + port + ": " + statusName);
                    continue;
                }
                lines.Add(indent + "port " + port + ": " + statusName + " speed=" + (speed < Speeds.Length ? Speeds[speed] : speed.ToString()) +
                    " vid:pid=" + vid.ToString("x4") + ":" + pid.ToString("x4") + " bcdUSB=" + bcdUsb.ToString("x4") + " hub=" + isHub + " address=" + address);
                if (isHub) {
                    var name = new byte[1024];
                    BitConverter.GetBytes(port).CopyTo(name, 0);
                    if (DeviceIoControl(hub, IOCTL_USB_GET_NODE_CONNECTION_NAME, name, name.Length, name, name.Length, out returned, IntPtr.Zero)) {
                        int nameLength = BitConverter.ToInt32(name, 4);
                        string child = Encoding.Unicode.GetString(name, 8, nameLength - 4).TrimEnd('\0');
                        Hub(@"\\.\" + child, depth + 1, lines);
                    }
                }
            }
        }
    }
}
'@
[UsbTree]::Walk()
