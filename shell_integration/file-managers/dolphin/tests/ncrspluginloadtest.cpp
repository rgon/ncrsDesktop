// SPDX-License-Identifier: GPL-3.0-or-later
//
// Loads the built .so the way Dolphin does (KPluginMetaData + QPluginLoader,
// then qobject_cast to KOverlayIconPlugin).
#include <KOverlayIconPlugin>
#include <KPluginMetaData>

#include <QPluginLoader>
#include <QTemporaryDir>
#include <QTest>
#include <QUrl>

class NcrsPluginLoadTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void loadsAsOverlayIconPlugin()
    {
        QTemporaryDir runtime; // keep the singleton away from any real socket
        qputenv("XDG_RUNTIME_DIR", runtime.path().toLocal8Bit());

        const QString path = QStringLiteral(NCRS_PLUGIN_FILE);
        QVERIFY2(path.endsWith(QLatin1String("/ncrsoverlayplugin.so")), qPrintable(path));
        const KPluginMetaData md(path);
        QVERIFY(md.isValid());
        QCOMPARE(md.pluginId(), QStringLiteral("ncrsoverlayplugin"));

        QPluginLoader loader(path);
        QObject *instance = loader.instance();
        QVERIFY2(instance, qPrintable(loader.errorString()));
        auto *plugin = qobject_cast<KOverlayIconPlugin *>(instance);
        QVERIFY(plugin);
        // No daemon: answers immediately and empty.
        QVERIFY(plugin->getOverlays(QUrl::fromLocalFile(QStringLiteral("/home/u/Nextcloud/a"))).isEmpty());
    }
};

QTEST_GUILESS_MAIN(NcrsPluginLoadTest)
#include "ncrspluginloadtest.moc"
