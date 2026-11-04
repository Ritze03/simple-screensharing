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
 * One-shot modes, next to the long-running instance: `scroll N` (wheel) and `drag` (a real pointer drag:
 * `swaymsg seat - cursor set` sends the client no motion while a button is down, so a drag has to come from a
 * pointer device; see main()).
 *
 * Built on demand by hs-rig.sh (wayland-scanner + gcc, see hs-lib.sh hs_build_vptr).
 * Runs until the compositor goes away or it is killed. Talks to $WAYLAND_DISPLAY
 * only (hs-lib.sh always sets it to the ABSOLUTE socket path of the private sway).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
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

static uint32_t now_ms(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return (uint32_t)(t.tv_sec * 1000 + t.tv_nsec / 1000000);
}

int main(int argc, char **argv) {
  struct wl_display *dpy = wl_display_connect(NULL);
  if (!dpy) { fprintf(stderr, "hs-vptr: cannot connect to $WAYLAND_DISPLAY\n"); return 1; }
  struct wl_registry *reg = wl_display_get_registry(dpy);
  wl_registry_add_listener(reg, &rl, NULL);
  wl_display_roundtrip(dpy);
  if (!mgr || !seat) { fprintf(stderr, "hs-vptr: compositor lacks zwlr_virtual_pointer_manager_v1 or wl_seat\n"); return 1; }
  struct zwlr_virtual_pointer_v1 *vp = zwlr_virtual_pointer_manager_v1_create_virtual_pointer(mgr, seat);
  if (argc == 3 && !strcmp(argv[1], "scroll")) {
    /* One-shot: `hs-vptr scroll N` sends N wheel notches (N > 0 = wheel down, < 0 = up) at the seat's
     * current cursor position (put it there with `swaymsg seat - cursor set X Y`), then exits. The
     * long-running instance keeps the seat's pointer capability meanwhile. Task #113. */
    int n = atoi(argv[2]), dir = n < 0 ? -1 : 1;
    for (int i = 0; i < abs(n); i++) {
      zwlr_virtual_pointer_v1_axis_source(vp, 0 /* wheel */);
      zwlr_virtual_pointer_v1_axis_discrete(vp, 1000 * (i + 1), 0 /* vertical */, wl_fixed_from_int(15 * dir), dir);
      zwlr_virtual_pointer_v1_frame(vp);
      wl_display_roundtrip(dpy);
    }
    zwlr_virtual_pointer_v1_destroy(vp);
    wl_display_roundtrip(dpy);
    return 0;
  }
  if (argc >= 8 && !strcmp(argv[1], "drag")) {
    /* One-shot: `hs-vptr drag W H X1 Y1 X2 Y2 [STEPS [HOLD_MS]]`: press at (X1,Y1), move to (X2,Y2) in STEPS
     * steps, stay pressed for HOLD_MS (time to look at the drag: a dump, a screenshot, a stream ending), release.
     * Coordinates are global pixels, W x H the size of the whole output layout (the virtual pointer's absolute
     * motion is a fraction of that). */
    uint32_t w = atoi(argv[2]), h = atoi(argv[3]);
    int x1 = atoi(argv[4]), y1 = atoi(argv[5]), x2 = atoi(argv[6]), y2 = atoi(argv[7]);
    int steps = argc > 8 ? atoi(argv[8]) : 20, hold = argc > 9 ? atoi(argv[9]) : 0;
    enum { BTN_LEFT = 0x110 };
    zwlr_virtual_pointer_v1_motion_absolute(vp, now_ms(), x1, y1, w, h);
    zwlr_virtual_pointer_v1_frame(vp);
    wl_display_roundtrip(dpy);
    usleep(150000);
    zwlr_virtual_pointer_v1_button(vp, now_ms(), BTN_LEFT, 1);
    zwlr_virtual_pointer_v1_frame(vp);
    wl_display_roundtrip(dpy);
    usleep(100000);
    for (int i = 1; i <= steps; i++) {
      zwlr_virtual_pointer_v1_motion_absolute(vp, now_ms(), x1 + (x2 - x1) * i / steps, y1 + (y2 - y1) * i / steps, w, h);
      zwlr_virtual_pointer_v1_frame(vp);
      wl_display_roundtrip(dpy);
      usleep(16000);
    }
    usleep(100000 + hold * 1000);
    zwlr_virtual_pointer_v1_button(vp, now_ms(), BTN_LEFT, 0);
    zwlr_virtual_pointer_v1_frame(vp);
    wl_display_roundtrip(dpy);
    usleep(50000);
    zwlr_virtual_pointer_v1_destroy(vp);
    wl_display_roundtrip(dpy);
    return 0;
  }
  wl_display_roundtrip(dpy);
  fprintf(stderr, "hs-vptr: virtual pointer up\n");
  while (wl_display_dispatch(dpy) != -1) {}
  return 0;
}
