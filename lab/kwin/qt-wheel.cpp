// Lab client for scenarios/bridge-wheel-qt.sh: logs QWheelEvent deltas.
#include <QApplication>
#include <QWidget>
#include <QWheelEvent>
#include <QTimer>
#include <cstdio>
class W : public QWidget {
protected:
    void wheelEvent(QWheelEvent *e) override {
        printf("qt wheel angleDelta=%d,%d pixelDelta=%d,%d phase=%d source=%d\n", e->angleDelta().x(), e->angleDelta().y(),
               e->pixelDelta().x(), e->pixelDelta().y(), int(e->phase()), int(e->source()));
        fflush(stdout);
    }
};
int main(int argc, char **argv) {
    QApplication a(argc, argv);
    W w; w.showFullScreen();
    QTimer::singleShot(12000, &a, &QApplication::quit);
    return a.exec();
}
