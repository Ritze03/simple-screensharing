/*
 * hs-vptr.c — keep a zwlr_virtual_pointer_v1 alive on the headless sway.
 *
 * Why: headless sway has no input devices, so its seat advertises NO pointer
 * capability (swaymsg -t get_seats: "capabilities":0) and clients never get a
 * wl_pointer: `swaymsg seat - cursor set/press` then moves a cursor nobody
 * listens to. A virtual pointer device makes the seat grow the capability for as
 * long as this process holds it; the actual moving/clicking is still done with
 * `swaymsg seat - cursor set X Y` / `cursor press|release button1` (hs-drive.sh).
 *
 * Built on demand by hs-rig.sh (wayland-scanner + gcc, see hs-lib.sh hs_build_vptr).
 * Runs until the compositor goes away or it is killed. Talks to $WAYLAND_DISPLAY
 * only (hs-lib.sh always sets it to the ABSOLUTE socket path of the private sway).
 */
#include <stdio.h>
#include <string.h>
#include <wayland-client.h>
#include "hs-vptr-protocol.h"

static struct zwlr_virtual_pointer_manager_v1 *mgr;
static struct wl_seat *seat;

static void on_global(void *d, struct wl_registry *r, uint32_t name, const char *iface, uint32_t ver) {
  (void)d; (void)ver;
  if (!strcmp(iface, zwlr_virtual_pointer_manager_v1_interface.name))
    mgr = wl_registry_bind(r, name, &zwlr_virtual_pointer_manager_v1_interface, 1);
  else if (!strcmp(iface, "wl_seat") && !seat)
    seat = wl_registry_bind(r, name, &wl_seat_interface, 1);
}
static void on_remove(void *d, struct wl_registry *r, uint32_t name) { (void)d; (void)r; (void)name; }
static const struct wl_registry_listener rl = { on_global, on_remove };

int main(void) {
  struct wl_display *dpy = wl_display_connect(NULL);
  if (!dpy) { fprintf(stderr, "hs-vptr: cannot connect to $WAYLAND_DISPLAY\n"); return 1; }
  struct wl_registry *reg = wl_display_get_registry(dpy);
  wl_registry_add_listener(reg, &rl, NULL);
  wl_display_roundtrip(dpy);
  if (!mgr || !seat) { fprintf(stderr, "hs-vptr: compositor lacks zwlr_virtual_pointer_manager_v1 or wl_seat\n"); return 1; }
  zwlr_virtual_pointer_manager_v1_create_virtual_pointer(mgr, seat);
  wl_display_roundtrip(dpy);
  fprintf(stderr, "hs-vptr: virtual pointer up\n");
  while (wl_display_dispatch(dpy) != -1) {}
  return 0;
}
