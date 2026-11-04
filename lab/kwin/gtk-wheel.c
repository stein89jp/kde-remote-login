/* Lab client for scenarios/bridge-wheel-gtk.sh: logs GTK 3 smooth-scroll deltas. */
#include <gtk/gtk.h>
#include <stdio.h>
static gboolean on_scroll(GtkWidget *w, GdkEventScroll *e, gpointer d) {
    double dx = 0, dy = 0;
    if (e->direction == GDK_SCROLL_SMOOTH) gdk_event_get_scroll_deltas((GdkEvent *)e, &dx, &dy);
    printf("gtk3 scroll direction=%d delta=%.3f,%.3f\n", e->direction, dx, dy);
    fflush(stdout);
    return TRUE;
}
static gboolean quit(gpointer d) { gtk_main_quit(); return FALSE; }
int main(int argc, char **argv) {
    gtk_init(&argc, &argv);
    GtkWidget *win = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_widget_add_events(win, GDK_SCROLL_MASK | GDK_SMOOTH_SCROLL_MASK);
    g_signal_connect(win, "scroll-event", G_CALLBACK(on_scroll), NULL);
    gtk_window_fullscreen(GTK_WINDOW(win));
    gtk_widget_show_all(win);
    g_timeout_add(12000, quit, NULL);
    gtk_main();
    return 0;
}
