// SPDX-License-Identifier: GPL-3.0-or-later
#include "ncrsemblems.h"

#include "ncrsclient.h"

// Only names Breeze ships in its `emblems` context at 8, 16 and 22 px (plus
// @2x/@3x; Breeze 6 adds 24 px) are used, so emblems stay crisp at every
// Dolphin zoom level. Breeze has neither `emblem-default`, `emblem-downloads`
// nor a full-colour `emblem-synchronizing`, which the Nautilus adapter uses
// under Adwaita.
//
//   kept      emblem-checked       green rounded square with a tick: pinned
//   cached    vcs-normal           green circle with a tick: local, evictable
//   transfers vcs-update-required  amber circle with arrows (what Dolphin's
//                                  VCS plugins and Nextcloud's client use)
//   partial   emblem-information   blue: "some children are local"; a warning
//                                  emblem would read as an error
//   shared    emblem-shared
//
// `synced`, `remote` and `unknown` draw nothing, matching the Nautilus adapter:
// online-only is the default state of a virtual filesystem and should be quiet.
const NcrsStatusEmblem kNcrsStatusEmblems[] = {
    {"kept", "emblem-checked"},
    {"cached", "vcs-normal"},
    {"synced", nullptr},
    {"remote", nullptr},
    {"downloading", "vcs-update-required"},
    {"uploading", "vcs-update-required"},
    {"pending", "vcs-update-required"}, // queued upload: same arrows as Nautilus
    {"partial", "emblem-information"},
    {"unknown", nullptr},
};
const int kNcrsStatusEmblemCount = int(sizeof(kNcrsStatusEmblems) / sizeof(kNcrsStatusEmblems[0]));
const char kNcrsSharedEmblem[] = "emblem-shared";

static const NcrsStatusEmblem *find(const QString &status)
{
    for (int i = 0; i < kNcrsStatusEmblemCount; ++i) {
        if (status == QLatin1String(kNcrsStatusEmblems[i].status))
            return &kNcrsStatusEmblems[i];
    }
    return nullptr;
}

bool ncrsIsKnownStatus(const QString &status)
{
    return find(status) != nullptr;
}

QString ncrsEmblemForStatus(const QString &status)
{
    const NcrsStatusEmblem *e = find(status);
    return e && e->emblem ? QString::fromLatin1(e->emblem) : QString();
}

QStringList ncrsOverlays(const NcrsEntry &entry)
{
    QStringList overlays;
    const QString emblem = ncrsEmblemForStatus(entry.status);
    if (!emblem.isEmpty())
        overlays << emblem;
    if (entry.isShared())
        overlays << QString::fromLatin1(kNcrsSharedEmblem);
    return overlays;
}
