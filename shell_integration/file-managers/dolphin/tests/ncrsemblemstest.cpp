// SPDX-License-Identifier: GPL-3.0-or-later
#include "ncrsclient.h"
#include "ncrsemblems.h"

#include <QFile>
#include <QSet>
#include <QTest>

class NcrsEmblemsTest : public QObject
{
    Q_OBJECT

    static QStringList vocabulary()
    {
        QFile f(QStringLiteral(NCRS_STATUS_VOCABULARY));
        if (!f.open(QIODevice::ReadOnly | QIODevice::Text))
            return {};
        QStringList words;
        while (!f.atEnd()) {
            const QString line = QString::fromUtf8(f.readLine()).trimmed();
            if (!line.isEmpty() && !line.startsWith(QLatin1Char('#')))
                words << line;
        }
        return words;
    }

private Q_SLOTS:
    void everyVocabularyWordIsMapped()
    {
        const QStringList words = vocabulary();
        QVERIFY2(!words.isEmpty(), "cannot read " NCRS_STATUS_VOCABULARY);
        for (const QString &w : words)
            QVERIFY2(ncrsIsKnownStatus(w), qPrintable(QStringLiteral("status '%1' has no emblem decision").arg(w)));
    }

    void tableHasNoStaleWords()
    {
        const QStringList vocab = vocabulary();
        const QSet<QString> words(vocab.cbegin(), vocab.cend());
        QSet<QString> seen;
        for (int i = 0; i < kNcrsStatusEmblemCount; ++i) {
            const QString s = QString::fromLatin1(kNcrsStatusEmblems[i].status);
            QVERIFY2(words.contains(s), qPrintable(QStringLiteral("'%1' is not in status-vocabulary.txt").arg(s)));
            QVERIFY2(!seen.contains(s), qPrintable(QStringLiteral("'%1' mapped twice").arg(s)));
            seen.insert(s);
        }
    }

    void quietStatusesDrawNothing()
    {
        for (const char *s : {"synced", "remote", "unknown", "", "no-such-status"})
            QVERIFY2(ncrsEmblemForStatus(QString::fromLatin1(s)).isEmpty(), s);
    }

    void overlaysOrderStatusThenShared()
    {
        QCOMPARE(ncrsOverlays({QStringLiteral("kept"), QString()}), QStringList{QStringLiteral("emblem-checked")});
        QCOMPARE(ncrsOverlays({QStringLiteral("uploading"), QStringLiteral("Shared with you")}),
                 (QStringList{QStringLiteral("vcs-update-required"), QStringLiteral("emblem-shared")}));
        QCOMPARE(ncrsOverlays({QStringLiteral("remote"), QStringLiteral("Shared")}), QStringList{QStringLiteral("emblem-shared")});
        QVERIFY(ncrsOverlays({}).isEmpty());
    }

    void emblemsExistInBreeze()
    {
        const QString base = QStringLiteral(NCRS_BREEZE_ICON_DIR "/emblems");
        if (!QFile::exists(base))
            QSKIP("Breeze icon theme not installed");
        QStringList names{QString::fromLatin1(kNcrsSharedEmblem)};
        for (int i = 0; i < kNcrsStatusEmblemCount; ++i) {
            if (kNcrsStatusEmblems[i].emblem)
                names << QString::fromLatin1(kNcrsStatusEmblems[i].emblem);
        }
        // Every emblem size Breeze 5 and 6 both ship (Breeze 6 adds 24 px).
        for (const QString &name : std::as_const(names)) {
            for (const char *size : {"8", "16", "22"}) {
                const QString path = QStringLiteral("%1/%2/%3.svg").arg(base, QLatin1String(size), name);
                QVERIFY2(QFile::exists(path), qPrintable(path + QStringLiteral(" missing")));
            }
        }
    }
};

QTEST_GUILESS_MAIN(NcrsEmblemsTest)
#include "ncrsemblemstest.moc"
