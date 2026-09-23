// SPDX-License-Identifier: GPL-3.0-or-later
#include "fakencrsserver.h"
#include "ncrsclient.h"

#include <QSignalSpy>
#include <QTemporaryDir>
#include <QTest>

namespace {
const QString kMount = QStringLiteral("/home/u/Nextcloud");
const QString kDocs = kMount + QStringLiteral("/Docs");
}

class NcrsClientTest : public QObject
{
    Q_OBJECT

private:
    QTemporaryDir m_tmp;
    QString m_sock;
    FakeNcrsServer *m_server = nullptr;

    NcrsClient::Options options(int ttlMs = 30000) const
    {
        NcrsClient::Options o;
        o.socketPath = m_sock;
        o.clientId = QStringLiteral("dolphin-test");
        o.cacheTtlMs = ttlMs;
        o.minBackoffMs = 20;
        o.maxBackoffMs = 80;
        o.refetchDelayMs = 10;
        return o;
    }

    // Looks `path` up until its parent listing has arrived; returns the hit.
    static bool fetched(NcrsClient &c, const QString &path, NcrsEntry *out)
    {
        QSignalSpy spy(&c, &NcrsClient::directoryFetched);
        if (c.lookup(path, out))
            return true;
        if (!spy.wait(2000) && spy.isEmpty())
            return false;
        return c.lookup(path, out);
    }

private Q_SLOTS:
    void init()
    {
        m_sock = m_tmp.filePath(QStringLiteral("ncrs-%1.sock").arg(QLatin1String(QTest::currentTestFunction())));
        m_server = new FakeNcrsServer(kMount, this);
        QVERIFY(m_server->listen(m_sock));
    }

    void cleanup()
    {
        delete m_server;
        m_server = nullptr;
    }

    void helloParsesMountVersionAndCapabilities()
    {
        m_server->setHelloReply(QStringLiteral("OK\t3\t0.1.75\t/home/u/Nextcloud/\tdetaildir,watch,,keep"));
        NcrsClient c(options());
        QSignalSpy connectedSpy(&c, &NcrsClient::connected);
        QVERIFY(!c.isConnected());
        QVERIFY(!c.lookup(QStringLiteral("/tmp/elsewhere"), nullptr)); // any local path triggers the connect
        QVERIFY(connectedSpy.wait(2000));
        QCOMPARE(m_server->commands.value(0), QStringLiteral("HELLO dolphin-test 3"));
        QCOMPARE(c.mountPoint(), kMount); // trailing slash stripped
        QCOMPARE(c.daemonVersion(), QStringLiteral("0.1.75"));
        QCOMPARE(c.capabilities(), (QStringList{QStringLiteral("detaildir"), QStringLiteral("watch"), QStringLiteral("keep")}));
        QVERIFY(c.isUnderMount(kDocs));
        QVERIFY(c.isUnderMount(kMount));
        QVERIFY(!c.isUnderMount(kMount + QStringLiteral("2/file")));
    }

    void helloRejectedByOldDaemonStaysDisconnected()
    {
        m_server->setHelloReply(QStringLiteral("unknown"));
        NcrsClient c(options());
        c.lookup(kDocs + QStringLiteral("/a.txt"), nullptr);
        QTRY_COMPARE_WITH_TIMEOUT(m_server->count(QStringLiteral("HELLO ")), 1, 2000);
        QTest::qWait(50);
        QVERIFY(!c.isConnected());
        QCOMPARE(m_server->count(QStringLiteral("DETAILDIR ")), 0);
    }

    void absentSocketFailsCheaplyThenRetries()
    {
        m_server->close();
        QFile::remove(m_sock);
        NcrsClient c(options());
        QElapsedTimer t;
        t.start();
        for (int i = 0; i < 1000; ++i)
            QVERIFY(!c.lookup(kDocs + QStringLiteral("/f%1").arg(i), nullptr));
        QVERIFY2(t.elapsed() < 1000, "lookups must not block while the daemon is down");
        QVERIFY(!c.isConnected());

        // The daemon appears: the pending lookup is answered without another call.
        m_server->setDir(kDocs, {{QStringLiteral("f1"), QStringLiteral("kept"), {}}});
        QVERIFY(m_server->listen(m_sock));
        QSignalSpy changed(&c, &NcrsClient::entryChanged);
        QTRY_VERIFY_WITH_TIMEOUT(c.isConnected(), 2000);
        QTRY_VERIFY_WITH_TIMEOUT(!changed.isEmpty(), 2000);
        QCOMPARE(changed.first().at(0).toString(), kDocs + QStringLiteral("/f1"));
    }

    void detailDirRecordsAreParsed()
    {
        const QChar rs(0x1e);
        m_server->setRawDirReply(kDocs,
                                 QStringList{
                                     QStringLiteral("a.txt\tkept\t\tRGDNVW\talice\t10"),
                                     QStringLiteral("b.txt\tremote\tShared with you\tR\tbob\t0"),
                                     QStringLiteral("c dir\tpartial\tShared by you\tRGDNVCK\talice\t0"),
                                     QStringLiteral("d.txt\tuploading,shared"), // STATUS-style spelling, short record
                                     QStringLiteral("garbage"),
                                     QString(),
                                 }
                                     .join(rs));
        NcrsClient c(options());
        QHash<QString, NcrsEntry> seen;
        connect(&c, &NcrsClient::entryChanged, this, [&](const QString &p, const NcrsEntry &, const NcrsEntry &after) {
            seen.insert(p, after);
        });
        NcrsEntry e;
        QVERIFY(fetched(c, kDocs + QStringLiteral("/a.txt"), &e));
        QCOMPARE(e.status, QStringLiteral("kept"));
        QVERIFY(!e.isShared());

        QVERIFY(c.lookup(kDocs + QStringLiteral("/b.txt"), &e));
        QCOMPARE(e.status, QStringLiteral("remote"));
        QCOMPARE(e.sharing, QStringLiteral("Shared with you"));
        QVERIFY(c.lookup(kDocs + QStringLiteral("/c dir"), &e));
        QCOMPARE(e.status, QStringLiteral("partial"));
        QVERIFY(c.lookup(kDocs + QStringLiteral("/d.txt"), &e));
        QCOMPARE(e.status, QStringLiteral("uploading"));
        QVERIFY(e.isShared());
        QVERIFY(!c.lookup(kDocs + QStringLiteral("/garbage"), &e));
        QCOMPARE(seen.size(), 4);
        QCOMPARE(m_server->count(QStringLiteral("DETAILDIR ")), 1);
        QCOMPARE(m_server->commands.filter(QStringLiteral("DETAILDIR ")).first(), QStringLiteral("DETAILDIR ") + kDocs);
    }

    void cacheHitMissAndTtl()
    {
        m_server->setDir(kDocs, {{QStringLiteral("a.txt"), QStringLiteral("cached"), {}}});
        NcrsClient c(options(/*ttlMs=*/300));
        NcrsEntry e;
        QVERIFY(!c.lookup(kDocs + QStringLiteral("/a.txt"), &e)); // cold miss
        QVERIFY(fetched(c, kDocs + QStringLiteral("/a.txt"), &e));
        QCOMPARE(e.status, QStringLiteral("cached"));
        QCOMPARE(m_server->count(QStringLiteral("DETAILDIR ")), 1);

        // Fresh hits and unknown children cost nothing.
        for (int i = 0; i < 50; ++i)
            QVERIFY(c.lookup(kDocs + QStringLiteral("/a.txt"), &e));
        QVERIFY(!c.lookup(kDocs + QStringLiteral("/missing"), &e));
        QTest::qWait(30);
        QCOMPARE(m_server->count(QStringLiteral("DETAILDIR ")), 1);

        // Outside the mount, and the mount root itself: never queried.
        QVERIFY(!c.lookup(QStringLiteral("/etc/passwd"), &e));
        QVERIFY(!c.lookup(kMount, &e));
        QTest::qWait(30);
        QCOMPARE(m_server->count(QStringLiteral("DETAILDIR ")), 1);

        // Past the TTL: the stale entry still answers, and one refetch goes out.
        m_server->setDir(kDocs, {{QStringLiteral("a.txt"), QStringLiteral("kept"), {}}});
        QTest::qWait(350);
        QVERIFY(c.lookup(kDocs + QStringLiteral("/a.txt"), &e));
        QCOMPARE(e.status, QStringLiteral("cached"));
        QVERIFY(c.lookup(kDocs + QStringLiteral("/a.txt"), &e));
        QTRY_COMPARE_WITH_TIMEOUT(m_server->count(QStringLiteral("DETAILDIR ")), 2, 2000);
        QTRY_VERIFY_WITH_TIMEOUT(c.lookup(kDocs + QStringLiteral("/a.txt"), &e) && e.status == QLatin1String("kept"), 2000);
    }

    void watchRecordsInvalidateAndReemit()
    {
        const QString file = kDocs + QStringLiteral("/a.txt");
        m_server->setDir(kDocs, {{QStringLiteral("a.txt"), QStringLiteral("remote"), {}}});
        NcrsClient c(options());
        QSignalSpy watching(&c, &NcrsClient::watching);
        NcrsEntry e;
        QVERIFY(fetched(c, file, &e));
        QVERIFY(watching.count() || watching.wait(2000));

        QList<std::pair<QString, QString>> changes; // path, new status
        connect(&c, &NcrsClient::entryChanged, this, [&](const QString &p, const NcrsEntry &, const NcrsEntry &a) {
            changes.append({p, a.status});
        });

        const QList<std::pair<QString, QString>> steps = {
            {QStringLiteral("S:") + file, QStringLiteral("downloading")},
            {QStringLiteral("M:") + file, QStringLiteral("kept")},
            {QStringLiteral("A:") + file, QStringLiteral("uploading")},
        };
        for (const auto &[record, status] : steps) {
            changes.clear();
            m_server->setDir(kDocs, {{QStringLiteral("a.txt"), status, {}}});
            m_server->pushEvents({record});
            QTRY_COMPARE_WITH_TIMEOUT(changes.size(), 1, 2000);
            QCOMPARE(changes.first(), std::make_pair(file, status));
        }

        // A burst of records for one directory costs a single DETAILDIR.
        const int before = m_server->count(QStringLiteral("DETAILDIR "));
        m_server->pushEvents({QStringLiteral("S:") + file, QStringLiteral("S:") + file, QStringLiteral("M:") + file});
        QTRY_COMPARE_WITH_TIMEOUT(m_server->count(QStringLiteral("DETAILDIR ")), before + 1, 2000);
        QTest::qWait(50);
        QCOMPARE(m_server->count(QStringLiteral("DETAILDIR ")), before + 1);

        // Records for directories nobody looked at are ignored.
        m_server->pushEvents({QStringLiteral("S:") + kMount + QStringLiteral("/Other/x")});
        QTest::qWait(50);
        QCOMPARE(m_server->count(QStringLiteral("DETAILDIR ")), before + 1);
    }

    void renameInvalidatesBothParents()
    {
        const QString other = kMount + QStringLiteral("/Other");
        m_server->setDir(kDocs, {{QStringLiteral("a.txt"), QStringLiteral("kept"), {}}});
        m_server->setDir(other, {});
        NcrsClient c(options());
        NcrsEntry e;
        QVERIFY(fetched(c, kDocs + QStringLiteral("/a.txt"), &e));
        QVERIFY(!fetched(c, other + QStringLiteral("/a.txt"), &e));
        QTRY_VERIFY_WITH_TIMEOUT(m_server->watcherCount() == 1, 2000);

        QList<std::pair<QString, QString>> changes;
        connect(&c, &NcrsClient::entryChanged, this, [&](const QString &p, const NcrsEntry &, const NcrsEntry &a) {
            changes.append({p, a.status});
        });
        m_server->setDir(kDocs, {});
        m_server->setDir(other, {{QStringLiteral("a.txt"), QStringLiteral("kept"), {}}});
        m_server->pushEvents({QStringLiteral("R:%1/a.txt%2%3/a.txt").arg(kDocs, QChar(0x1e), other)});
        QTRY_COMPARE_WITH_TIMEOUT(changes.size(), 2, 2000);
        QVERIFY(changes.contains(std::make_pair(QString(kDocs + QStringLiteral("/a.txt")), QString())));
        QVERIFY(changes.contains(std::make_pair(QString(other + QStringLiteral("/a.txt")), QStringLiteral("kept"))));
    }

    void resyncRefetchesEveryCachedDirectory()
    {
        const QString other = kMount + QStringLiteral("/Other");
        m_server->setDir(kDocs, {{QStringLiteral("a"), QStringLiteral("remote"), {}}});
        m_server->setDir(other, {{QStringLiteral("b"), QStringLiteral("remote"), {}}});
        NcrsClient c(options());
        NcrsEntry e;
        QVERIFY(fetched(c, kDocs + QStringLiteral("/a"), &e));
        QVERIFY(fetched(c, other + QStringLiteral("/b"), &e));
        QTRY_VERIFY_WITH_TIMEOUT(m_server->watcherCount() == 1, 2000);

        QSet<QString> changed;
        connect(&c, &NcrsClient::entryChanged, this, [&](const QString &p, const NcrsEntry &, const NcrsEntry &) {
            changed.insert(p);
        });
        m_server->setDir(kDocs, {{QStringLiteral("a"), QStringLiteral("cached"), {}}});
        m_server->setDir(other, {{QStringLiteral("b"), QStringLiteral("kept"), QStringLiteral("Shared")}});
        m_server->pushEvents({QStringLiteral("RESYNC")});
        QTRY_COMPARE_WITH_TIMEOUT(changed.size(), 2, 2000);
        QVERIFY(c.lookup(other + QStringLiteral("/b"), &e));
        QCOMPARE(e.status, QStringLiteral("kept"));
        QVERIFY(e.isShared());
    }

    void watchResumesFromLastSequence()
    {
        m_server->setDir(kDocs, {{QStringLiteral("a"), QStringLiteral("remote"), {}}});
        NcrsClient c(options());
        NcrsEntry e;
        QVERIFY(fetched(c, kDocs + QStringLiteral("/a"), &e));
        QTRY_VERIFY_WITH_TIMEOUT(m_server->watcherCount() == 1, 2000);
        m_server->pushEvents({QStringLiteral("S:/elsewhere"), QStringLiteral("S:/elsewhere2")});
        QTest::qWait(50);

        m_server->dropWatchers();
        QTRY_VERIFY_WITH_TIMEOUT(m_server->watcherCount() == 1, 2000);
        QCOMPARE(m_server->commands.filter(QStringLiteral("WATCH")).last(), QStringLiteral("WATCH %1").arg(m_server->seq));
    }

    void daemonRestartReconnects()
    {
        m_server->setDir(kDocs, {{QStringLiteral("a"), QStringLiteral("kept"), {}}});
        NcrsClient c(options());
        QSignalSpy disconnected(&c, &NcrsClient::disconnected);
        NcrsEntry e;
        QVERIFY(fetched(c, kDocs + QStringLiteral("/a"), &e));

        m_server->close();
        QVERIFY(disconnected.wait(2000));
        QVERIFY(!c.isConnected());
        QVERIFY(m_server->listen(m_sock));
        const int hellos = m_server->count(QStringLiteral("HELLO "));
        QVERIFY(!c.lookup(kDocs + QStringLiteral("/a"), &e)); // cache dropped with the connection
        QTRY_VERIFY_WITH_TIMEOUT(c.isConnected(), 2000);
        QCOMPARE(m_server->count(QStringLiteral("HELLO ")), hellos + 1);
        QVERIFY(fetched(c, kDocs + QStringLiteral("/a"), &e));
        QCOMPARE(e.status, QStringLiteral("kept"));
    }
};

QTEST_GUILESS_MAIN(NcrsClientTest)
#include "ncrsclienttest.moc"
