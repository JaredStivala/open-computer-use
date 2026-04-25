#include <X11/Xlib.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(void) {
  Display *display = XOpenDisplay(NULL);
  if (!display) {
    fprintf(stderr, "XOpenDisplay failed\n");
    return 1;
  }

  int screen = DefaultScreen(display);
  Window root = RootWindow(display, screen);
  unsigned long black = BlackPixel(display, screen);
  unsigned long white = WhitePixel(display, screen);
  Window window = XCreateSimpleWindow(display, root, 0, 0, 300, 200, 1, black, white);
  XStoreName(display, window, "agent-pixel-probe");
  XSelectInput(display, window, ExposureMask | ButtonPressMask | StructureNotifyMask);
  XMapRaised(display, window);
  XFlush(display);

  int toggled = 0;
  for (;;) {
    XEvent event;
    XNextEvent(display, &event);
    if (event.type == MapNotify) {
      puts("ready");
      fflush(stdout);
    } else if (event.type == Expose) {
      XSetForeground(display, DefaultGC(display, screen), toggled ? black : white);
      XFillRectangle(display, window, DefaultGC(display, screen), 0, 0, 300, 200);
      XFlush(display);
    } else if (event.type == ButtonPress) {
      toggled = !toggled;
      XSetForeground(display, DefaultGC(display, screen), toggled ? black : white);
      XFillRectangle(display, window, DefaultGC(display, screen), 0, 0, 300, 200);
      XFlush(display);
    }
  }
}
