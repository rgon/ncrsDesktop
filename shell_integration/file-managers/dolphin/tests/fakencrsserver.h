// SPDX-License-Identifier: GPL-3.0-or-later
//
// A QLocalServer speaking just enough of ncrs IPC protocol v3 for the tests:
// HELLO, DETAILDIR (canned replies) and WATCH (records pushed by the test).
#pragma once

#include <QHash>
#include <QList>
#include <QLocalServer>
#include <QLocalSocket>
#include <QObject>
#include <QStringList>

class FakeNcrsServer : public QObject
{
    Q_OBJECT
public:
    explicit FakeNcrsServer(const QString &mountPoint, QObject *parent = nullptr);
    ~FakeNcrsServer() override { close(); } // sockets outlive m_buf otherwise

    bool listen(const QString &socketPath);
    void close();                 // stop listening and drop every connection
    void dropWatchers();          // drop WATCH streams only

    // DETAILDIR reply builder: children as {basename, status, sharing}.
    struct Child {
        QString name, status, sharing;
    };
    void setDir(const QString &dir, const QList<Child> &children);
    void setRawDirReply(const QString &dir, const QString &reply);
    void setHelloReply(const QString &reply) { m_helloReply = reply; }

    // Writes `EV\t<seq>\t<records…>` to every WATCH stream.
    void pushEvents(const QStringList &records);
    int watcherCount() const { return m_watchers.size(); }

    QStringList commands;         // every request line received, in order
    int count(const QString &prefix) const;
    quint64 seq = 0;

private:
    void onNewConnection();
    void onReadyRead(QLocalSocket *sock);

    QString m_mount;
    QString m_helloReply;
    QLocalServer m_server;
    QHash<QString, QString> m_dirReplies;
    QList<QLocalSocket *> m_watchers;
    QHash<QLocalSocket *, QByteArray> m_buf;
};
