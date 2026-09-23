// SPDX-License-Identifier: GPL-3.0-or-later
#include "ncrsclient.h"

#include <QDir>
#include <QFileInfo>
#include <QLoggingCategory>

#include <unistd.h>

Q_LOGGING_CATEGORY(lcNcrs, "ncrs.dolphin", QtWarningMsg)

namespace {

const QChar kRecordSep(0x1e);

QString parentOf(const QString &path)
{
    const int slash = path.lastIndexOf(QLatin1Char('/'));
    if (slash <= 0)
        return QStringLiteral("/");
    return path.left(slash);
}

// One DETAILDIR child: `basename\tstatus\tsharing\tperms\towner\tsize`.
bool parseDetailRecord(const QString &record, QString *name, NcrsEntry *entry)
{
    const QStringList f = record.split(QLatin1Char('\t'));
    if (f.size() < 2 || f[0].isEmpty())
        return false;
    *name = f[0];
    entry->sharing = f.value(2);
    // Tolerate the STATUS spelling (`kept,shared`) in case a daemon ever
    // folds sharing into the status field.
    const QStringList words = f[1].split(QLatin1Char(','));
    entry->status = words.first();
    if (entry->sharing.isEmpty() && words.contains(QLatin1String("shared")))
        entry->sharing = QStringLiteral("Shared");
    return true;
}

} // namespace

NcrsClient::NcrsClient(const Options &options, QObject *parent)
    : QObject(parent)
    , m_opt(options)
    , m_backoffMs(options.minBackoffMs)
    , m_watchBackoffMs(options.minBackoffMs)
{
    qRegisterMetaType<NcrsEntry>();
    m_clock.start();

    m_retryTimer.setSingleShot(true);
    connect(&m_retryTimer, &QTimer::timeout, this, [this] {
        // Only reconnect when something is waiting for an answer.
        if (!m_wantedBeforeHello.isEmpty())
            ensureConnected();
    });

    m_watchRetryTimer.setSingleShot(true);
    connect(&m_watchRetryTimer, &QTimer::timeout, this, &NcrsClient::startWatch);

    m_refetchTimer.setSingleShot(true);
    connect(&m_refetchTimer, &QTimer::timeout, this, &NcrsClient::flushRefetches);
}

NcrsClient::~NcrsClient()
{
    dropConnection();
}

QString NcrsClient::defaultSocketPath()
{
    // Mirrors socket_dir() in ncrs_core/src/ipc.rs.
    const QByteArray runtime = qgetenv("XDG_RUNTIME_DIR");
    if (!runtime.isEmpty())
        return QString::fromLocal8Bit(runtime) + QStringLiteral("/ncrs.sock");
    return QDir::tempPath() + QStringLiteral("/ncrs-%1/ncrs.sock").arg(::getuid());
}

NcrsClient *NcrsClient::instance()
{
    // Deliberately leaked: Dolphin never unloads overlay plugins, and tearing
    // sockets down after QCoreApplication is gone buys nothing.
    static NcrsClient *client = [] {
        Options opt;
        opt.socketPath = defaultSocketPath();
        opt.clientId = QStringLiteral("dolphin-kf%1").arg(QT_VERSION_MAJOR);
        return new NcrsClient(opt);
    }();
    return client;
}

bool NcrsClient::isUnderMount(const QString &path) const
{
    return !m_mountPoint.isEmpty() && (path == m_mountPoint || path.startsWith(m_mountPrefix));
}

bool NcrsClient::lookup(const QString &path, NcrsEntry *out)
{
    if (!path.startsWith(QLatin1Char('/')))
        return false;
    const QString parent = parentOf(path);

    if (m_state != State::Ready) {
        // The mount point arrives with HELLO, so any local URL may be under it.
        if (m_wantedBeforeHello.size() < m_opt.maxCachedDirs)
            m_wantedBeforeHello.insert(parent);
        ensureConnected();
        return false;
    }
    // Entries come from the parent's listing, so the mount root itself has none.
    if (path == m_mountPoint || !isUnderMount(parent))
        return false;

    auto it = m_dirs.find(parent);
    if (it == m_dirs.end()) {
        requestDir(parent);
        return false;
    }
    it->lastUsedMs = nowMs();
    const bool fresh = it->fetchedAtMs >= 0 && nowMs() - it->fetchedAtMs < m_opt.cacheTtlMs;
    // Look the child up before requestDir(), which may rehash m_dirs.
    const auto child = it->children.constFind(path.mid(parent.size() + 1));
    const bool hit = child != it->children.constEnd();
    if (hit && out)
        *out = *child;
    if (!fresh && !it->inFlight)
        requestDir(parent); // stale-while-revalidate: answer now, correct later
    return hit;
}

// ── Query connection ─────────────────────────────────────────────────────────

void NcrsClient::ensureConnected()
{
    if (m_state != State::Idle || m_retryTimer.isActive())
        return;
    // A stat is far cheaper than a failing connect() in every KDE process.
    if (!QFileInfo::exists(m_opt.socketPath)) {
        scheduleRetry();
        return;
    }
    m_state = State::Connecting;
    m_query = new QLocalSocket(this);
    connect(m_query, &QLocalSocket::connected, this, &NcrsClient::onQueryConnected);
    connect(m_query, &QLocalSocket::readyRead, this, &NcrsClient::onQueryReadyRead);
    // Queued: QLocalSocket may report a refused connect from inside
    // connectToServer(), and teardown must not run under its feet.
    connect(m_query, &QLocalSocket::errorOccurred, this, &NcrsClient::onQueryError, Qt::QueuedConnection);
    connect(m_query, &QLocalSocket::disconnected, this, &NcrsClient::onQueryError, Qt::QueuedConnection);
    m_query->connectToServer(m_opt.socketPath);
}

void NcrsClient::onQueryConnected()
{
    m_state = State::Handshaking;
    m_inFlight.enqueue({RequestKind::Hello, QString()});
    m_query->write(QStringLiteral("HELLO %1 %2\n").arg(m_opt.clientId).arg(kProtocolVersion).toUtf8());
}

void NcrsClient::onQueryError()
{
    if (!m_query || sender() != m_query)
        return; // a signal queued before the socket was dropped
    const bool wasReady = m_state == State::Ready;
    qCDebug(lcNcrs) << "query connection lost:" << (m_query ? m_query->errorString() : QString());
    dropConnection();
    if (wasReady)
        m_backoffMs = m_opt.minBackoffMs; // the daemon restarted; come back quickly
    scheduleRetry();
}

void NcrsClient::onQueryReadyRead()
{
    if (!m_query)
        return;
    m_queryBuf += m_query->readAll();
    int nl;
    while (m_query && (nl = m_queryBuf.indexOf('\n')) >= 0) {
        QString line = QString::fromUtf8(m_queryBuf.constData(), nl);
        m_queryBuf.remove(0, nl + 1);
        if (line.endsWith(QLatin1Char('\r')))
            line.chop(1);
        if (m_inFlight.isEmpty()) {
            qCWarning(lcNcrs) << "unsolicited reply ignored:" << line.left(80);
            continue;
        }
        handleReply(m_inFlight.dequeue(), line);
    }
}

void NcrsClient::handleReply(const Request &req, const QString &line)
{
    switch (req.kind) {
    case RequestKind::Hello:
        handleHello(line);
        break;
    case RequestKind::DetailDir:
        applyDetailDir(req.arg, line);
        break;
    }
}

void NcrsClient::handleHello(const QString &line)
{
    // OK\t<proto>\t<package-version>\t<mount-point>\t<capabilities>
    const QStringList f = line.split(QLatin1Char('\t'));
    QString mount = f.value(3);
    while (mount.size() > 1 && mount.endsWith(QLatin1Char('/')))
        mount.chop(1);
    if (f.value(0) != QLatin1String("OK") || !mount.startsWith(QLatin1Char('/'))) {
        // A pre-v3 daemon answers `unknown` and cannot tell us the mount.
        qCWarning(lcNcrs) << "ncrs handshake rejected:" << line.left(120);
        dropConnection();
        m_backoffMs = m_opt.maxBackoffMs;
        scheduleRetry();
        return;
    }
    if (f.value(1).toInt() != kProtocolVersion)
        qCInfo(lcNcrs) << "ncrs speaks protocol" << f.value(1) << "; this adapter speaks" << kProtocolVersion;

    m_mountPoint = mount;
    m_mountPrefix = mount == QLatin1String("/") ? mount : mount + QLatin1Char('/');
    m_daemonVersion = f.value(2);
    m_capabilities = f.value(4).split(QLatin1Char(','), Qt::SkipEmptyParts);
    m_state = State::Ready;
    m_backoffMs = m_opt.minBackoffMs;
    qCDebug(lcNcrs) << "connected to ncrs" << m_daemonVersion << "mount" << m_mountPoint;
    Q_EMIT connected();

    const QSet<QString> wanted = std::exchange(m_wantedBeforeHello, {});
    for (const QString &dir : wanted) {
        if (isUnderMount(dir))
            requestDir(dir);
    }
    pumpQueue();
}

void NcrsClient::requestDir(const QString &dir)
{
    DirCache &d = m_dirs[dir];
    d.lastUsedMs = nowMs();
    if (d.inFlight)
        return;
    d.inFlight = true;
    send(RequestKind::DetailDir, dir);
    evictIfNeeded();
    if (!m_watch && !m_watchRetryTimer.isActive())
        startWatch(); // only processes that browse the mount hold a feed
}

void NcrsClient::applyDetailDir(const QString &dir, const QString &line)
{
    auto it = m_dirs.find(dir);
    if (it == m_dirs.end())
        return; // evicted while in flight
    it->inFlight = false;
    it->fetchedAtMs = nowMs();

    struct Change {
        QString path;
        NcrsEntry before, after;
    };
    QList<Change> changes;
    if (line.startsWith(QLatin1String("error:")) || line == QLatin1String("unknown")) {
        // Keep whatever we had; the TTL paces the next attempt.
        qCDebug(lcNcrs) << "DETAILDIR" << dir << "->" << line.left(120);
    } else {
        QHash<QString, NcrsEntry> fresh;
        const QStringList records = line.split(kRecordSep, Qt::SkipEmptyParts);
        fresh.reserve(records.size());
        for (const QString &rec : records) {
            QString name;
            NcrsEntry e;
            if (parseDetailRecord(rec, &name, &e))
                fresh.insert(name, e);
        }
        for (auto f = fresh.cbegin(); f != fresh.cend(); ++f) {
            const auto old = it->children.constFind(f.key());
            if (old == it->children.cend() || *old != f.value())
                changes.append({dir + QLatin1Char('/') + f.key(),
                                old == it->children.cend() ? NcrsEntry() : *old, f.value()});
        }
        for (auto o = it->children.cbegin(); o != it->children.cend(); ++o) {
            if (!fresh.contains(o.key()))
                changes.append({dir + QLatin1Char('/') + o.key(), o.value(), NcrsEntry()});
        }
        it->children = std::move(fresh);
    }
    if (std::exchange(it->refetchQueued, false))
        scheduleRefetch(dir);

    // Emit last: receivers may call lookup(), which can rehash m_dirs.
    for (const Change &c : std::as_const(changes))
        Q_EMIT entryChanged(c.path, c.before, c.after);
    Q_EMIT directoryFetched(dir);
}

void NcrsClient::send(RequestKind kind, const QString &arg)
{
    m_queue.enqueue({kind, arg});
    pumpQueue();
}

void NcrsClient::pumpQueue()
{
    // The daemon answers each line in order, so requests are pipelined and
    // replies matched FIFO.
    if (m_state != State::Ready || !m_query)
        return;
    while (!m_queue.isEmpty()) {
        const Request req = m_queue.dequeue();
        QString line;
        switch (req.kind) {
        case RequestKind::Hello:
            continue;
        case RequestKind::DetailDir:
            line = QStringLiteral("DETAILDIR ") + req.arg;
            break;
        }
        m_inFlight.enqueue(req);
        m_query->write(line.toUtf8() + '\n');
    }
}

void NcrsClient::dropConnection()
{
    const bool wasReady = m_state == State::Ready;
    m_state = State::Idle;
    for (QLocalSocket **s : {&m_query, &m_watch}) {
        if (*s) {
            (*s)->disconnect(this);
            (*s)->abort();
            (*s)->deleteLater();
            *s = nullptr;
        }
    }
    m_queue.clear();
    m_inFlight.clear();
    m_queryBuf.clear();
    m_watchBuf.clear();
    m_watchRetryTimer.stop();
    m_refetchTimer.stop();
    m_refetch.clear();
    // Statuses may have moved on while we were away; start from scratch.
    m_dirs.clear();
    m_haveWatchSeq = false;
    if (wasReady)
        Q_EMIT disconnected();
}

void NcrsClient::scheduleRetry()
{
    m_retryTimer.start(m_backoffMs);
    m_backoffMs = qMin(m_backoffMs * 2, m_opt.maxBackoffMs);
}

// ── Change feed (second connection) ──────────────────────────────────────────

void NcrsClient::startWatch()
{
    if (m_watch || m_state != State::Ready)
        return;
    m_watch = new QLocalSocket(this);
    connect(m_watch, &QLocalSocket::connected, this, [this] {
        // Resume where the previous stream stopped; the daemon replays the
        // gap or answers RESYNC.
        const QString cmd = m_haveWatchSeq ? QStringLiteral("WATCH %1\n").arg(m_watchSeq) : QStringLiteral("WATCH\n");
        m_watch->write(cmd.toUtf8());
    });
    connect(m_watch, &QLocalSocket::readyRead, this, &NcrsClient::onWatchReadyRead);
    connect(m_watch, &QLocalSocket::errorOccurred, this, &NcrsClient::onWatchLost, Qt::QueuedConnection);
    connect(m_watch, &QLocalSocket::disconnected, this, &NcrsClient::onWatchLost, Qt::QueuedConnection);
    m_watch->connectToServer(m_opt.socketPath);
}

void NcrsClient::onWatchLost()
{
    if (sender() != m_watch || !m_watch)
        return;
    m_watch->disconnect(this);
    m_watch->abort();
    m_watch->deleteLater();
    m_watch = nullptr;
    m_watchBuf.clear();
    if (m_state != State::Ready)
        return; // the query connection's retry brings everything back
    m_watchRetryTimer.start(m_watchBackoffMs);
    m_watchBackoffMs = qMin(m_watchBackoffMs * 2, m_opt.maxBackoffMs);
}

void NcrsClient::onWatchReadyRead()
{
    if (!m_watch)
        return;
    m_watchBuf += m_watch->readAll();
    int nl;
    while (m_watch && (nl = m_watchBuf.indexOf('\n')) >= 0) {
        QString line = QString::fromUtf8(m_watchBuf.constData(), nl);
        m_watchBuf.remove(0, nl + 1);
        if (line.endsWith(QLatin1Char('\r')))
            line.chop(1);
        handleWatchLine(line);
    }
}

void NcrsClient::handleWatchLine(const QString &line)
{
    const QStringList f = line.split(QLatin1Char('\t'));
    const QString &tag = f.first();
    if (tag == QLatin1String("PING"))
        return;
    if (tag == QLatin1String("WATCHING")) {
        m_watchSeq = f.value(1).toULongLong();
        m_haveWatchSeq = true;
        m_watchBackoffMs = m_opt.minBackoffMs;
        Q_EMIT watching(m_watchSeq);
        return;
    }
    if (tag != QLatin1String("EV")) {
        qCDebug(lcNcrs) << "unexpected WATCH line:" << line.left(80);
        return;
    }
    // EV\t<next>\t<record>\t<record>…   (or EV\t<next>\tRESYNC)
    bool ok = false;
    const quint64 next = f.value(1).toULongLong(&ok);
    if (ok) {
        m_watchSeq = next;
        m_haveWatchSeq = true;
    }
    for (int i = 2; i < f.size(); ++i) {
        const QString &rec = f[i];
        if (rec == QLatin1String("RESYNC")) {
            invalidateAll();
            continue;
        }
        const int colon = rec.indexOf(QLatin1Char(':'));
        if (colon <= 0)
            continue;
        const QStringView kind = QStringView(rec).left(colon);
        const QString body = rec.mid(colon + 1);
        if (kind == QLatin1String("R")) {
            for (const QString &p : body.split(kRecordSep, Qt::SkipEmptyParts))
                invalidatePath(p);
        } else if (kind == QLatin1String("S") || kind == QLatin1String("A") || kind == QLatin1String("D")
                   || kind == QLatin1String("M") || kind == QLatin1String("DA") || kind == QLatin1String("DD")) {
            invalidatePath(body);
        }
    }
}

void NcrsClient::invalidatePath(const QString &path)
{
    if (!isUnderMount(path) || path == m_mountPoint)
        return;
    // The parent's listing holds the path's own status; each cached ancestor's
    // listing may hold a directory whose `partial` state just flipped.
    QString dir = parentOf(path);
    while (true) {
        auto it = m_dirs.find(dir);
        if (it != m_dirs.end()) {
            it->fetchedAtMs = -1;
            scheduleRefetch(dir);
        }
        if (dir == m_mountPoint || !isUnderMount(dir))
            break;
        dir = parentOf(dir);
    }
}

void NcrsClient::invalidateAll()
{
    for (auto it = m_dirs.begin(); it != m_dirs.end(); ++it) {
        it->fetchedAtMs = -1;
        m_refetch.insert(it.key());
    }
    if (!m_refetch.isEmpty() && !m_refetchTimer.isActive())
        m_refetchTimer.start(m_opt.refetchDelayMs);
}

void NcrsClient::scheduleRefetch(const QString &dir)
{
    m_refetch.insert(dir);
    if (!m_refetchTimer.isActive())
        m_refetchTimer.start(m_opt.refetchDelayMs);
}

void NcrsClient::flushRefetches()
{
    const QSet<QString> dirs = std::exchange(m_refetch, {});
    for (const QString &dir : dirs) {
        auto it = m_dirs.find(dir);
        if (it == m_dirs.end())
            continue;
        if (it->inFlight)
            it->refetchQueued = true; // the reply in flight may predate the change
        else
            requestDir(dir);
    }
}

void NcrsClient::evictIfNeeded()
{
    while (m_dirs.size() > m_opt.maxCachedDirs) {
        auto victim = m_dirs.end();
        for (auto it = m_dirs.begin(); it != m_dirs.end(); ++it) {
            if (!it->inFlight && (victim == m_dirs.end() || it->lastUsedMs < victim->lastUsedMs))
                victim = it;
        }
        if (victim == m_dirs.end())
            return;
        m_dirs.erase(victim);
    }
}
