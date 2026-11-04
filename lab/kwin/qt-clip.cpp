// Lab client for scenarios/bridge-clipboard.sh: a focused fullscreen Qt 6
// window that logs the native selection (text, HTML, image) whenever it
// changes, and once it has seen the bridge's "from-rdp" text, sets its own
// text + HTML selection, then an image-only one, for the bridge to read
// back on the next two mouse presses (wl_data_device.set_selection needs a fresh input serial; the
// press comes from kwin-input-probe).
#include <QApplication>
#include <QClipboard>
#include <QImage>
#include <QMimeData>
#include <QTimer>
#include <QMouseEvent>
#include <QWidget>
#include <cstdio>
static bool seen;
static int answers;
/* First press: text + HTML (the bridge prefers those over an image, as
 * before). Second press: an image alone. */
static void answer()
{
    if (!seen || answers >= 2)
        return;
    QClipboard *c = QGuiApplication::clipboard();
    auto *data = new QMimeData;
    if (answers++ == 0) {
        data->setText(QString::fromUtf8("from-qt: wörld ✓"));
        data->setHtml("<i>from-qt</i> rich");
    } else {
        QImage img(3, 2, QImage::Format_ARGB32);
        img.fill(QColor(10, 200, 30));
        data->setImageData(img);
    }
    c->setMimeData(data);
    printf("qt-clip set own selection %d\n", answers);
    fflush(stdout);
}
class W : public QWidget
{
protected:
    void mousePressEvent(QMouseEvent *) override { answer(); }
};
static void dump(QClipboard *c)
{
    const QMimeData *m = c->mimeData();
    if (!m)
        return;
    QImage image = qvariant_cast<QImage>(m->imageData());
    printf("qt-clip selection formats=[%s] text=\"%s\" html=\"%s\" image=%dx%d pixel0=%08x\n",
           m->formats().join(",").toUtf8().constData(), m->text().toUtf8().constData(),
           m->html().toUtf8().constData(), image.width(), image.height(),
           image.isNull() ? 0u : image.pixel(0, 0));
    fflush(stdout);
    if (m->text().startsWith("from-rdp"))
        seen = true;
}
int main(int argc, char **argv)
{
    QApplication a(argc, argv);
    W w;
    w.showFullScreen();
    QClipboard *c = QGuiApplication::clipboard();
    QObject::connect(c, &QClipboard::dataChanged, [c] { dump(c); });
    QTimer::singleShot(25000, &a, &QApplication::quit);
    return a.exec();
}
