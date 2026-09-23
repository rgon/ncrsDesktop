// SPDX-License-Identifier: GPL-3.0-or-later
#include "fakencrsserver.h"

FakeNcrsServer::FakeNcrsServer(const QString &mountPoint, QObject *parent)
    : QObject(parent)
    , m_mount(mountPoint)
    , m_helloReply(QStringLiteral("OK\t3\t0.0.0-test\t%1\tdetaildir,events,watch,weburl,keep,evict").arg(mountPoint))
{
    connect(&m_server, &QLocalServer::newConnection, this, &FakeNcrsServer::onNewConnection);
}

bool FakeNcrsServer::listen(const QString &socketPath)
{
    QLocalServer::removeServer(socketPath);
    return m_server.listen(socketPath);
}

void FakeNcrsServer::close()
{
    m_server.close();
    const auto socks = m_buf.keys();
    for (QLocalSocket *s : socks) {
        s->disconnect(this);
        s->abort();
        s->deleteLater();
    }
    m_buf.clear();
    m_watchers.clear();
}

void FakeNcrsServer::dropWatchers()
{
    for (QLocalSocket *s : std::as_const(m_watchers)) {
        s->disconnect(this);
        m_buf.remove(s);
        s->abort();
        s->deleteLater();
    }
    m_watchers.clear();
}

void FakeNcrsServer::setDir(const QString &dir, const QList<Child> &children)
{
    QStringList records;
    for (const Child &c : children)
        records << QStringList{c.name, c.status, c.sharing, QStringLiteral("RGDNVW"), QStringLiteral("alice"), QStringLiteral("42")}
                       .join(QLatin1Char('\t'));
    m_dirReplies.insert(dir, records.join(QChar(0x1e)));
}

void FakeNcrsServer::setRawDirReply(const QString &dir, const QString &reply)
{
    m_dirReplies.insert(dir, reply);
}

void FakeNcrsServer::pushEvents(const QStringList &records)
{
    seq += records.size();
    const QByteArray line = (QStringList{QStringLiteral("EV"), QString::number(seq)} + records).join(QLatin1Char('\t')).toUtf8() + '\n';
    for (QLocalSocket *s : std::as_const(m_watchers))
        s->write(line);
}

int FakeNcrsServer::count(const QString &prefix) const
{
    int n = 0;
    for (const QString &c : commands)
        n += c.startsWith(prefix) ? 1 : 0;
    return n;
}

void FakeNcrsServer::onNewConnection()
{
    while (QLocalSocket *s = m_server.nextPendingConnection()) {
        m_buf.insert(s, {});
        connect(s, &QLocalSocket::readyRead, this, [this, s] { onReadyRead(s); });
        connect(s, &QLocalSocket::disconnected, this, [this, s] {
            m_buf.remove(s);
            m_watchers.removeAll(s);
            s->deleteLater();
        });
    }
}

void FakeNcrsServer::onReadyRead(QLocalSocket *sock)
{
    QByteArray &buf = m_buf[sock];
    buf += sock->readAll();
    int nl;
    while ((nl = buf.indexOf('\n')) >= 0) {
        const QString line = QString::fromUtf8(buf.left(nl));
        buf.remove(0, nl + 1);
        commands << line;
        QString reply;
        if (line.startsWith(QLatin1String("HELLO "))) {
            reply = m_helloReply;
        } else if (line.startsWith(QLatin1String("DETAILDIR "))) {
            reply = m_dirReplies.value(line.mid(10));
        } else if (line == QLatin1String("WATCH") || line.startsWith(QLatin1String("WATCH "))) {
            m_watchers << sock;
            reply = QStringLiteral("WATCHING\t%1").arg(seq);
        } else {
            reply = QStringLiteral("unknown");
        }
        sock->write(reply.toUtf8() + '\n');
    }
}
