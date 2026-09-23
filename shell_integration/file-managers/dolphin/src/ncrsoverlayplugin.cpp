// SPDX-License-Identifier: GPL-3.0-or-later
#include "ncrsoverlayplugin.h"

#include "ncrsclient.h"
#include "ncrsemblems.h"

#include <QUrl>

NcrsOverlayPlugin::NcrsOverlayPlugin(QObject *parent)
    : NcrsOverlayPlugin(NcrsClient::instance(), parent)
{
}

NcrsOverlayPlugin::NcrsOverlayPlugin(NcrsClient *client, QObject *parent)
    : KOverlayIconPlugin(parent)
    , m_client(client)
{
    connect(m_client, &NcrsClient::entryChanged, this, &NcrsOverlayPlugin::onEntryChanged);
}

QStringList NcrsOverlayPlugin::getOverlays(const QUrl &item)
{
    if (!item.isLocalFile())
        return {};
    QString path = item.toLocalFile();
    while (path.size() > 1 && path.endsWith(QLatin1Char('/')))
        path.chop(1);
    NcrsEntry entry;
    if (!m_client->lookup(path, &entry))
        return {};
    return ncrsOverlays(entry);
}

void NcrsOverlayPlugin::onEntryChanged(const QString &path, const NcrsEntry &before, const NcrsEntry &after)
{
    // A fresh listing reports every child; only repaint the ones whose
    // emblems actually differ from what Dolphin was last told.
    const QStringList overlays = ncrsOverlays(after);
    if (overlays != ncrsOverlays(before))
        Q_EMIT overlaysChanged(QUrl::fromLocalFile(path), overlays);
}
