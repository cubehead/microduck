# Radxa Zero 3W offline first-time Wi-Fi setup

The optional `duck-wifi-setup` image component is independent of the
interactive `webd` console. It takes no camera, motion, servo, SSH, or update
requests and does not alter the BLE configuration or Wi-Fi PSK RPC.

On a Zero 3W with NetworkManager managing AP-capable `wlan0`, no existing
Wi-Fi profile, and no in-progress join, the unit waits up to 95 seconds for
the managed radio on cold boot and offers a temporary encrypted
`duck-xxxx-setup` hotspot for 15 minutes per boot. The separate setup passphrase
is generated from kernel entropy for that boot and displayed on the local
physical HDMI terminal (`tty1`). An administrator at a local console can also
read `sudo cat /run/microduck-wifi-setup/access` during the window.
Do not send this password to anyone or copy it into logs. The password is not
a duck pairing PIN, a setup code, or the target network password.

1. Join the setup hotspot from a phone/computer, and stay connected even if the
   OS labels it “no internet”. Open **http://10.42.0.1:8090/** manually.
2. Enter a visible Wi-Fi SSID and its WPA personal passphrase; leave the
   passphrase blank for an open network. This portal makes no external requests.
   Hidden and enterprise networks must use another provisioning route.
3. The setup AP goes away while the single radio joins. If unsuccessful, within
   the remaining window the hotspot reappears with the same temporary WPA
   passphrase and the page displays a generic outcome. If a different channel
   configured the robot or the window expired, it does not reopen.
4. Verify connection and the assigned IP on the router or via the normal BLE
   network-status path. On the existing camera service, the usual LAN port is
   8080. A saved Wi-Fi profile prevents the AP from being offered at next boot.

The web setup listener binds both `wlan0` and the AP address; a temporary nft
table accepts only DHCP and the provisioning page from that interface and
blocks forwarding in both directions even when NetworkManager uses shared
mode. The AP profile is root-only under `/run`, is never set to autoconnect, and
is removed on exit. Web requests require the expected address/Host/Origin
and the current form token. The target Wi-Fi password is sent via configd's
local socket, not via command arguments or the journal, and only NetworkManager
keeps the resulting root-only known-network profile. No web handler calls
other configd methods. If NetworkManager cannot confirm the temporary AP has stopped, the service
leaves its firewall isolation in place rather than exposing other services
on a live hotspot. From the physical console, reboot the board to clear its
volatile setup state; do not clear the firewall while the AP is active.
Use BLE or a local terminal if the driver lacks AP
mode or if no display/console is available.

Image staging must install the dedicated binary as
`/usr/libexec/duck-wifi-setup`, enable the service, and apply the
`microduck-osupdate.socket.d` drop-in clearing the inherited wildcard listener
and binding the existing vendor updater to loopback. The updater is not
exposed to setup clients. This component does not enable the unauthenticated
developer motion console. AP activation/DHCP and Wi-Fi handoff require a
real Zero 3W bench test before claiming hardware support.
