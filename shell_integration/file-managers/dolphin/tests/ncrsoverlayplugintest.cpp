// SPDX-License-Identifier: GPL-3.0-or-later
#include "fakencrsserver.h"
#include "ncrsclient.h"
#include "ncrsoverlayplugin.h"

#include <QSignalSpy>
#include <QTemporaryDir>
#include <QTest>
#include <QUrl>

namespace {
const QString kMount = QStringLiteral("/home/u/Nextcloud");
const QString kDir = kMount + QStringLiteral("/Photos");
}

class NcrsOverlayPluginTest : public QObject
{
    Q_OBJECT

private:
    QTemporaryDir m_tmp;

    NcrsClient::Options options() const
    {
        NcrsClient::Options o;
        o.socketPath = m_tmp.filePath(QStringLiteral("ncrs-%1.sock").arg(QLatin1String(QTest::currentTestFunction())));
        o.clientId = QStringLiteral("dolphin-test");
        o.minBackoffMs = 20;
        o.maxBackoffMs = 80;
        o.refetchDelayMs = 10;
        return o;
    }

private Q_SLOTS:
    void missReturnsEmptyThenEmitsOverlays()
    {
        const NcrsClient::Options opt = options();
        FakeNcrsServer server(kMount);
        QVERIFY(server.listen(opt.socketPath));
        server.setDir(kDir, {
                                {QStringLiteral("kept.jpg"), QStringLiteral("kept"), {}},
                                {QStringLiteral("shared.jpg"), QStringLiteral("cached"), QStringLiteral("Shared by you")},
                                {QStringLiteral("online.jpg"), QStringLiteral("remote"), {}},
                            });
        NcrsClient client(opt);
        NcrsOverlayPlugin plugin(&client);
        QSignalSpy spy(&plugin, &KOverlayIconPlugin::overlaysChanged);

        const QUrl kept = QUrl::fromLocalFile(kDir + QStringLiteral("/kept.jpg"));
        QVERIFY(plugin.getOverlays(kept).isEmpty()); // cold: nothing yet, no blocking
        QTRY_COMPARE_WITH_TIMEOUT(spy.count(), 2, 2000);
        QTest::qWait(30);
        QCOMPARE(spy.count(), 2); // the emblem-less remote file is not re-announced

        QHash<QUrl, QStringList> emitted;
        for (const QList<QVariant> &args : std::as_const(spy))
            emitted.insert(args.at(0).toUrl(), args.at(1).toStringList());
        QCOMPARE(emitted.value(kept), QStringList{QStringLiteral("emblem-checked")});
        QCOMPARE(emitted.value(QUrl::fromLocalFile(kDir + QStringLiteral("/shared.jpg"))),
                 (QStringList{QStringLiteral("vcs-normal"), QStringLiteral("emblem-shared")}));

        QCOMPARE(plugin.getOverlays(kept), QStringList{QStringLiteral("emblem-checked")});
        QVERIFY(plugin.getOverlays(QUrl::fromLocalFile(kDir + QStringLiteral("/online.jpg"))).isEmpty());
        QVERIFY(plugin.getOverlays(QUrl(QStringLiteral("smb://host/share/kept.jpg"))).isEmpty());
        QVERIFY(plugin.getOverlays(QUrl::fromLocalFile(QStringLiteral("/etc/hosts"))).isEmpty());
    }

    void watchRecordRepaintsItem()
    {
        const NcrsClient::Options opt = options();
        FakeNcrsServer server(kMount);
        QVERIFY(server.listen(opt.socketPath));
        server.setDir(kDir, {{QStringLiteral("a.raw"), QStringLiteral("remote"), {}}});
        NcrsClient client(opt);
        NcrsOverlayPlugin plugin(&client);
        QSignalSpy fetched(&client, &NcrsClient::directoryFetched);
        const QUrl url = QUrl::fromLocalFile(kDir + QStringLiteral("/a.raw"));
        plugin.getOverlays(url);
        QVERIFY(fetched.wait(2000));
        QTRY_VERIFY_WITH_TIMEOUT(server.watcherCount() == 1, 2000);

        QSignalSpy spy(&plugin, &KOverlayIconPlugin::overlaysChanged);
        // "Keep" on the file: downloading, then kept.
        server.setDir(kDir, {{QStringLiteral("a.raw"), QStringLiteral("downloading"), {}}});
        server.pushEvents({QStringLiteral("S:") + url.toLocalFile()});
        QVERIFY(spy.wait(2000));
        QCOMPARE(spy.last().at(0).toUrl(), url);
        QCOMPARE(spy.last().at(1).toStringList(), QStringList{QStringLiteral("vcs-update-required")});

        server.setDir(kDir, {{QStringLiteral("a.raw"), QStringLiteral("kept"), {}}});
        server.pushEvents({QStringLiteral("S:") + url.toLocalFile()});
        QVERIFY(spy.wait(2000));
        QCOMPARE(spy.last().at(1).toStringList(), QStringList{QStringLiteral("emblem-checked")});

        // "Free up space": back to online-only clears the emblem.
        server.setDir(kDir, {{QStringLiteral("a.raw"), QStringLiteral("remote"), {}}});
        server.pushEvents({QStringLiteral("S:") + url.toLocalFile()});
        QVERIFY(spy.wait(2000));
        QVERIFY(spy.last().at(1).toStringList().isEmpty());
        QVERIFY(plugin.getOverlays(url).isEmpty());
    }

    void resyncRepaintsItems()
    {
        const NcrsClient::Options opt = options();
        FakeNcrsServer server(kMount);
        QVERIFY(server.listen(opt.socketPath));
        server.setDir(kDir, {{QStringLiteral("a"), QStringLiteral("remote"), {}}});
        NcrsClient client(opt);
        NcrsOverlayPlugin plugin(&client);
        QSignalSpy fetched(&client, &NcrsClient::directoryFetched);
        plugin.getOverlays(QUrl::fromLocalFile(kDir + QStringLiteral("/a")));
        QVERIFY(fetched.wait(2000));
        QTRY_VERIFY_WITH_TIMEOUT(server.watcherCount() == 1, 2000);

        QSignalSpy spy(&plugin, &KOverlayIconPlugin::overlaysChanged);
        server.setDir(kDir, {{QStringLiteral("a"), QStringLiteral("pending"), {}}});
        server.pushEvents({QStringLiteral("RESYNC")});
        QVERIFY(spy.wait(2000));
        QCOMPARE(spy.last().at(1).toStringList(), QStringList{QStringLiteral("vcs-update-required")});
    }
};

QTEST_GUILESS_MAIN(NcrsOverlayPluginTest)
#include "ncrsoverlayplugintest.moc"
