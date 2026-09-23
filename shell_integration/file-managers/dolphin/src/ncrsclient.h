// SPDX-License-Identifier: GPL-3.0-or-later
//
// Per-process client of the ncrs IPC socket (protocol v3, see
// shell_integration/file-managers/PROTOCOL.md).
//
// The overlay plugin is loaded into every Dolphin process, so this client
// stays idle until a local file URL is looked up, fails cheaply when the
// socket is absent, and never blocks the caller: lookups answer from a
// per-directory DETAILDIR cache and schedule a fetch on a miss.
#pragma once

#include <QElapsedTimer>
#include <QHash>
#include <QLocalSocket>
#include <QObject>
#include <QQueue>
#include <QSet>
#include <QTimer>

struct NcrsEntry {
    QString status;  // a word from status-vocabulary.txt, empty when unknown
    QString sharing; // "", "Shared by you", "Shared with you" or "Shared"

    bool isShared() const { return !sharing.isEmpty(); }
    bool operator==(const NcrsEntry &o) const { return status == o.status && sharing == o.sharing; }
    bool operator!=(const NcrsEntry &o) const { return !(*this == o); }
};
Q_DECLARE_METATYPE(NcrsEntry)

class NcrsClient : public QObject
{
    Q_OBJECT
public:
    struct Options {
        QString socketPath;
        QString clientId;          // "dolphin-kf6" / "dolphin-kf5"
        int cacheTtlMs = 30000;    // DETAILDIR freshness, as in the Nautilus adapter
        int minBackoffMs = 1000;   // first retry after a failed connect
        int maxBackoffMs = 60000;
        int refetchDelayMs = 150;  // coalesces bursts of change records per directory
        int maxCachedDirs = 256;
    };

    static constexpr int kProtocolVersion = 3;

    explicit NcrsClient(const Options &options, QObject *parent = nullptr);
    ~NcrsClient() override;

    // The process-wide client on $XDG_RUNTIME_DIR/ncrs.sock.
    static NcrsClient *instance();
    static QString defaultSocketPath();

    // Cached state of an absolute local path. Returns false on a miss (or
    // when the path is outside the mount) after scheduling whatever is needed
    // to answer later through entryChanged(): connecting, then a DETAILDIR of
    // the parent directory. Never blocks.
    bool lookup(const QString &path, NcrsEntry *out);

    bool isConnected() const { return m_state == State::Ready; }
    QString mountPoint() const { return m_mountPoint; }
    QString daemonVersion() const { return m_daemonVersion; }
    QStringList capabilities() const { return m_capabilities; }
    bool isUnderMount(const QString &path) const;

Q_SIGNALS:
    // A path's cached entry changed. `before` is empty when it was not cached.
    void entryChanged(const QString &path, const NcrsEntry &before, const NcrsEntry &after);
    void connected();
    void disconnected();
    // One DETAILDIR reply was applied (mainly for tests and debugging).
    void directoryFetched(const QString &dir);
    // The WATCH stream is up; `seq` is the sequence number it starts from.
    void watching(quint64 seq);

private:
    enum class State { Idle, Connecting, Handshaking, Ready };
    enum class RequestKind { Hello, DetailDir };
    struct Request {
        RequestKind kind;
        QString arg;
    };
    struct DirCache {
        QHash<QString, NcrsEntry> children; // basename → entry
        qint64 fetchedAtMs = -1;            // monotonic; -1 = never / invalidated
        bool inFlight = false;
        bool refetchQueued = false;         // a change arrived while in flight
        qint64 lastUsedMs = 0;
    };

    void ensureConnected();
    void onQueryConnected();
    void onQueryError();
    void onQueryReadyRead();
    void handleReply(const Request &req, const QString &line);
    void handleHello(const QString &line);
    void applyDetailDir(const QString &dir, const QString &line);
    void send(RequestKind kind, const QString &arg);
    void pumpQueue();
    void dropConnection();
    void scheduleRetry();

    void startWatch();
    void onWatchReadyRead();
    void onWatchLost();
    void handleWatchLine(const QString &line);
    void invalidatePath(const QString &path);
    void invalidateAll();
    void scheduleRefetch(const QString &dir);
    void flushRefetches();

    void requestDir(const QString &dir);
    void evictIfNeeded();
    qint64 nowMs() const { return m_clock.elapsed(); }

    Options m_opt;
    State m_state = State::Idle;
    QLocalSocket *m_query = nullptr;
    QLocalSocket *m_watch = nullptr;
    QQueue<Request> m_queue;       // not yet written
    QQueue<Request> m_inFlight;    // written, awaiting their one-line reply
    QByteArray m_queryBuf;
    QByteArray m_watchBuf;

    QString m_mountPoint;
    QString m_mountPrefix;         // m_mountPoint + '/'
    QString m_daemonVersion;
    QStringList m_capabilities;

    QHash<QString, DirCache> m_dirs;
    QSet<QString> m_wantedBeforeHello; // parents looked up while connecting
    QSet<QString> m_refetch;
    QTimer m_refetchTimer;

    QTimer m_retryTimer;
    int m_backoffMs;
    QTimer m_watchRetryTimer;
    int m_watchBackoffMs;
    quint64 m_watchSeq = 0;
    bool m_haveWatchSeq = false;

    QElapsedTimer m_clock;
};
