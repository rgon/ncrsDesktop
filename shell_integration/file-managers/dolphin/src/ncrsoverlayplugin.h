// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <KOverlayIconPlugin>

class NcrsClient;
struct NcrsEntry;

// Sync-status emblems for files under the ncrs mount.
//
// Dolphin loads the plugin root object with QPluginLoader and qobject_casts it
// to KOverlayIconPlugin, so this class carries Q_PLUGIN_METADATA itself rather
// than going through a KPluginFactory.
class NcrsOverlayPlugin : public KOverlayIconPlugin
{
    Q_OBJECT
    Q_PLUGIN_METADATA(IID "es.rgon.ncrs.KOverlayIconPlugin" FILE "ncrsoverlayplugin.json")
public:
    explicit NcrsOverlayPlugin(QObject *parent = nullptr);
    // For tests: use `client` instead of the process-wide one.
    explicit NcrsOverlayPlugin(NcrsClient *client, QObject *parent = nullptr);

    // Answers from the cache only; a miss returns nothing now and the emblems
    // arrive later through overlaysChanged().
    QStringList getOverlays(const QUrl &item) override;

private:
    void onEntryChanged(const QString &path, const NcrsEntry &before, const NcrsEntry &after);

    NcrsClient *m_client;
};
