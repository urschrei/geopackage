// The C++ half of the native provider. QGIS's class interface is C++, so the
// classes here inherit it and forward to Rust (lib.rs) through cxx. The one
// piece of real work done here is the per-feature loop in fetchFeature, which
// builds a QgsFeature straight from the Arrow arrays Rust holds, as OGR's
// provider builds one from an OGRFeature.

#include "geopackage-qgis/src/provider.h"
#include "geopackage-qgis/src/lib.rs.h"
#include "rust/cxx.h"

#include <memory>
#include <optional>
#include <vector>

#include <QDate>
#include <QDateTime>
#include <QStringList>

#include "qgsabstractgeometry.h"
#include "qgsapplication.h"
#include "qgscoordinatereferencesystem.h"
#include "qgscoordinatetransform.h"
#include "qgsexception.h"
#include "qgsfeature.h"
#include "qgsfeatureiterator.h"
#include "qgsfeaturerequest.h"
#include "qgsfields.h"
#include "qgsgeometry.h"
#include "qgsgeometryengine.h"
#include "qgsgeometryfactory.h"
#include "qgsmessagelog.h"
#include "qgsprovidermetadata.h"
#include "qgsproviderregistry.h"
#include "qgsrectangle.h"
#include "qgsvectordataprovider.h"
#include "qgswkbptr.h"
#include "qgswkbtypes.h"

// QGIS 3.36 moved the feature request enums into `Qgis`; 3.34 has only the
// class-scoped names. Everything version dependent goes through these.
#include "qgsconfig.h"
#if _QGIS_VERSION_INT >= 33600
#define GPKGRS_FLAG(name) Qgis::FeatureRequestFlag::name
#define GPKGRS_FILTER(name) Qgis::FeatureRequestFilterType::name
#else
#define GPKGRS_FLAG(name) QgsFeatureRequest::name
#define GPKGRS_FILTER(name) QgsFeatureRequest::Filter##name
#endif

namespace gpkgrs {
namespace {

const QString PROVIDER_KEY = QStringLiteral("geopackage_rs_native");
const QString PROVIDER_DESCRIPTION = QStringLiteral("GeoPackage (geopackage crate, native)");

// Matches `kind` in lib.rs.
enum Kind : uint8_t {
  Null = 0,
  Int = 1,
  Double = 2,
  Text = 3,
  Bool = 4,
  Blob = 5,
  Date = 6,
  DateTime = 7,
};

QString toQString(const rust::String &s) { return QString::fromUtf8(s.data(), static_cast<int>(s.size())); }
QString toQString(rust::Str s) { return QString::fromUtf8(s.data(), static_cast<int>(s.size())); }
rust::Str toStr(const QByteArray &bytes) { return rust::Str(bytes.constData(), static_cast<size_t>(bytes.size())); }

QVariant::Type variantType(uint8_t kind) {
  switch (kind) {
    case Int: return QVariant::LongLong;
    case Double: return QVariant::Double;
    case Bool: return QVariant::Bool;
    case Blob: return QVariant::ByteArray;
    case Date: return QVariant::Date;
    case DateTime: return QVariant::DateTime;
    default: return QVariant::String;
  }
}

struct Uri {
  QString path;
  QString layerName;
};

Uri decode(const QString &uri) {
  Uri parts;
  const QStringList pieces = uri.split('|');
  parts.path = pieces.value(0);
  for (int i = 1; i < pieces.size(); ++i) {
    const QString &piece = pieces.at(i);
    if (piece.startsWith(QStringLiteral("layername="), Qt::CaseInsensitive)) {
      parts.layerName = piece.mid(10);
    }
  }
  return parts;
}

// What the layer is, shared by the provider and every source it hands out.
struct Shared {
  rust::Box<RsLayer> layer;
  QgsFields fields;
  QgsCoordinateReferenceSystem crs;
  Qgis::WkbType wkbType = Qgis::WkbType::NoGeometry;
  bool hasGeometry = false;
};

class Source final : public QgsAbstractFeatureSource {
 public:
  Source(std::shared_ptr<Shared> shared, QString subset) : mShared(std::move(shared)), mSubset(std::move(subset)) {}
  QgsFeatureIterator getFeatures(const QgsFeatureRequest &request) override;

  std::shared_ptr<Shared> mShared;
  QString mSubset;
};

class Iterator final : public QgsAbstractFeatureIteratorFromSource<Source> {
 public:
  Iterator(Source *source, bool ownSource, const QgsFeatureRequest &request)
      : QgsAbstractFeatureIteratorFromSource<Source>(source, ownSource, request) {
    const Shared &shared = *mSource->mShared;
    if (mRequest.destinationCrs().isValid() && mRequest.destinationCrs() != shared.crs) {
      mTransform = QgsCoordinateTransform(shared.crs, mRequest.destinationCrs(), mRequest.transformContext());
    }
    try {
      mFilterRect = filterRectToSourceCrs(mTransform);
    } catch (QgsCsException &) {
      close();
      return;
    }
    if (!mFilterRect.isNull() && (mRequest.flags() & GPKGRS_FLAG(ExactIntersect))) {
      mExactGeometry = QgsGeometry::fromRect(mFilterRect);
      mExactEngine.reset(QgsGeometry::createGeometryEngine(mExactGeometry.constGet()));
      mExactEngine->prepareGeometry();
    }
    // An expression filter is evaluated by the base class on every row, and
    // may read any column, so it reads them all.
    const bool expression = mRequest.filterType() == GPKGRS_FILTER(Expression);
    mAllColumns = expression || !(mRequest.flags() & GPKGRS_FLAG(SubsetOfAttributes));
    if (!mAllColumns) {
      for (int index : mRequest.subsetOfAttributes()) {
        if (index >= 0 && index < shared.fields.count()) mColumns.push_back(static_cast<uint32_t>(index));
      }
    }
    mNoGeometry = (mRequest.flags() & GPKGRS_FLAG(NoGeometry)) && mFilterRect.isNull() && !mExactEngine;
    if (expression && mRequest.filterExpression() && mRequest.filterExpression()->needsGeometry()) {
      mNoGeometry = false;
    }
  }

  ~Iterator() override { close(); }

  bool rewind() override {
    if (mClosed) return false;
    mCursor.reset();
    mStarted = false;
    return true;
  }

  bool close() override {
    if (mClosed) return false;
    mCursor.reset();
    iteratorClosed();
    mClosed = true;
    return true;
  }

 protected:
  bool fetchFeature(QgsFeature &f) override {
    f.setValid(false);
    if (mClosed) return false;
    if (!mStarted && !start()) return false;
    const Shared &shared = *mSource->mShared;
    const int width = shared.fields.count();
    while (true) {
      bool more = false;
      try {
        more = (*mCursor)->advance();
      } catch (const rust::Error &error) {
        QgsMessageLog::logMessage(QString::fromUtf8(error.what()), PROVIDER_DESCRIPTION, Qgis::MessageLevel::Critical);
        more = false;
      }
      if (!more) {
        close();
        return false;
      }
      const RsCursor &cursor = **mCursor;
      f.setFields(shared.fields, false);
      f.setId(cursor.fid());

      QgsAttributes attributes(width);
      for (int i = 0; i < width; ++i) {
        const size_t field = static_cast<size_t>(i);
        switch (cursor.kind(field)) {
          case Int: attributes[i] = QVariant(static_cast<qlonglong>(cursor.int_value(field))); break;
          case Double: attributes[i] = QVariant(cursor.double_value(field)); break;
          case Text: attributes[i] = QVariant(toQString(cursor.text_value(field))); break;
          case Bool: attributes[i] = QVariant(cursor.int_value(field) != 0); break;
          case Blob: {
            const rust::Slice<const uint8_t> bytes = cursor.blob_value(field);
            attributes[i] = QVariant(QByteArray(reinterpret_cast<const char *>(bytes.data()), static_cast<int>(bytes.size())));
            break;
          }
          case Date: attributes[i] = QVariant(QDate(1970, 1, 1).addDays(cursor.int_value(field))); break;
          case DateTime:
            attributes[i] = QVariant(QDateTime::fromMSecsSinceEpoch(cursor.int_value(field) / 1000, Qt::UTC));
            break;
          default: attributes[i] = QVariant(variantType(shared.fields.at(i).type())); break;
        }
      }
      f.setAttributes(attributes);

      const rust::Slice<const uint8_t> wkb = cursor.wkb();
      if (!mNoGeometry && wkb.size() > 0) {
        QgsConstWkbPtr ptr(reinterpret_cast<const unsigned char *>(wkb.data()), static_cast<int>(wkb.size()));
        f.setGeometry(QgsGeometry(QgsGeometryFactory::geomFromWkb(ptr)));
      } else {
        f.clearGeometry();
      }

      if (mExactEngine && !(f.hasGeometry() && mExactEngine->intersects(f.geometry().constGet()))) continue;
      if (mTransform.isValid()) geometryToDestinationCrs(f, mTransform);
      f.setValid(true);
      return true;
    }
  }

 private:
  bool start() {
    mStarted = true;
    const Shared &shared = *mSource->mShared;
    QStringList clauses;
    if (!mSource->mSubset.isEmpty()) clauses << QStringLiteral("(%1)").arg(mSource->mSubset);
    if (mRequest.filterType() == GPKGRS_FILTER(Fid)) {
      clauses << QStringLiteral("fid = %1").arg(mRequest.filterFid());
    } else if (mRequest.filterType() == GPKGRS_FILTER(Fids)) {
      if (mRequest.filterFids().isEmpty()) {
        close();
        return false;
      }
      QStringList ids;
      for (QgsFeatureId id : mRequest.filterFids()) ids << QString::number(id);
      clauses << QStringLiteral("fid IN (%1)").arg(ids.join(','));
    }
    std::vector<double> bbox;
    if (!mFilterRect.isNull() && shared.hasGeometry) {
      bbox = {mFilterRect.xMinimum(), mFilterRect.yMinimum(), mFilterRect.xMaximum(), mFilterRect.yMaximum()};
    }
    const QByteArray where = clauses.join(QStringLiteral(" AND ")).toUtf8();
    // A read by id is small and known to be, so it skips the reader thread.
    const bool byId = mRequest.filterType() == GPKGRS_FILTER(Fid) || mRequest.filterType() == GPKGRS_FILTER(Fids);
    try {
      mCursor.emplace(shared.layer->start(rust::Slice<const double>(bbox.data(), bbox.size()), toStr(where),
                                          rust::Slice<const uint32_t>(mColumns.data(), mColumns.size()), mAllColumns,
                                          !mNoGeometry, byId));
    } catch (const rust::Error &error) {
      QgsMessageLog::logMessage(QString::fromUtf8(error.what()), PROVIDER_DESCRIPTION, Qgis::MessageLevel::Critical);
      close();
      return false;
    }
    return true;
  }

  QgsCoordinateTransform mTransform;
  QgsRectangle mFilterRect;
  QgsGeometry mExactGeometry;
  std::unique_ptr<QgsGeometryEngine> mExactEngine;
  std::vector<uint32_t> mColumns;
  bool mAllColumns = true;
  bool mNoGeometry = false;
  bool mStarted = false;
  bool mClosed = false;
  std::optional<rust::Box<RsCursor>> mCursor;
};

QgsFeatureIterator Source::getFeatures(const QgsFeatureRequest &request) {
  return QgsFeatureIterator(new Iterator(this, false, request));
}

class Provider final : public QgsVectorDataProvider {
 public:
  Provider(const QString &uri, const ProviderOptions &options, QgsDataProvider::ReadFlags flags)
      : QgsVectorDataProvider(uri, options, flags) {
    const Uri parts = decode(uri);
    try {
      const QByteArray path = parts.path.toUtf8();
      const QByteArray layer = parts.layerName.toUtf8();
      auto handle = open_layer(toStr(path), toStr(layer));
      const LayerInfo info = handle->info();
      auto shared = std::make_shared<Shared>(Shared{std::move(handle), QgsFields(), QgsCoordinateReferenceSystem(),
                                                    Qgis::WkbType::NoGeometry, info.has_geometry});
      for (const FieldInfo &field : info.fields) {
        QgsField qgsField(toQString(field.name), variantType(field.kind), toQString(field.declared_type), field.length);
        shared->fields.append(qgsField);
      }
      if (info.has_geometry) {
        shared->wkbType = QgsWkbTypes::parseType(toQString(info.wkb_type));
        if (!info.srs_authid.empty()) shared->crs = QgsCoordinateReferenceSystem(toQString(info.srs_authid));
        if (!shared->crs.isValid() && !info.srs_wkt.empty())
          shared->crs = QgsCoordinateReferenceSystem::fromWkt(toQString(info.srs_wkt));
      }
      mShared = std::move(shared);
      mValid = true;
    } catch (const rust::Error &error) {
      pushError(QStringLiteral("%1: cannot open %2: %3").arg(PROVIDER_DESCRIPTION, uri, QString::fromUtf8(error.what())));
    }
  }

  QgsAbstractFeatureSource *featureSource() const override { return new Source(mShared, mSubset); }
  QgsFeatureIterator getFeatures(const QgsFeatureRequest &request) const override {
    return QgsFeatureIterator(new Iterator(new Source(mShared, mSubset), true, request));
  }
  QString storageType() const override { return PROVIDER_DESCRIPTION; }
  Qgis::WkbType wkbType() const override { return mValid ? mShared->wkbType : Qgis::WkbType::Unknown; }
  QgsFields fields() const override { return mValid ? mShared->fields : QgsFields(); }
  QgsCoordinateReferenceSystem crs() const override { return mValid ? mShared->crs : QgsCoordinateReferenceSystem(); }
  bool isValid() const override { return mValid; }
  QString name() const override { return PROVIDER_KEY; }
  QString description() const override { return PROVIDER_DESCRIPTION; }
  QgsVectorDataProvider::Capabilities capabilities() const override { return QgsVectorDataProvider::SelectAtId; }

  long long featureCount() const override {
    if (!mValid) return 0;
    if (mCount < 0) {
      try {
        const QByteArray subset = mSubset.toUtf8();
        mCount = mShared->layer->count(toStr(subset));
      } catch (const rust::Error &) {
        mCount = 0;
      }
    }
    return mCount;
  }

  QgsRectangle extent() const override {
    if (!mValid || !mShared->hasGeometry) return QgsRectangle();
    if (!mExtent) {
      QgsRectangle rect;
      rect.setNull();
      double b[4] = {0, 0, 0, 0};
      bool recorded = false;
      if (mSubset.isEmpty()) {
        try {
          recorded = mShared->layer->extent(rust::Slice<double>(b, 4));
        } catch (const rust::Error &) {
          recorded = false;
        }
      }
      if (recorded) {
        rect = QgsRectangle(b[0], b[1], b[2], b[3]);
      } else {
        QgsFeatureRequest request;
        request.setNoAttributes();
        QgsFeatureIterator it = getFeatures(request);
        QgsFeature f;
        while (it.nextFeature(f)) {
          if (f.hasGeometry()) rect.combineExtentWith(f.geometry().boundingBox());
        }
      }
      mExtent = rect;
    }
    return *mExtent;
  }
  void updateExtents() override { mExtent.reset(); }

  bool supportsSubsetString() const override { return true; }
  QString subsetString() const override { return mSubset; }
  bool setSubsetString(const QString &subset, bool) override {
    const QString trimmed = subset.trimmed();
    if (trimmed == mSubset) return true;
    if (!trimmed.isEmpty()) {
      try {
        const QByteArray where = trimmed.toUtf8();
        auto probe = mShared->layer->start(rust::Slice<const double>(), toStr(where), rust::Slice<const uint32_t>(),
                                           false, false, false);
        // One batch shows the clause runs; dropping the probe stops the
        // reader rather than reading the whole subset.
        probe->advance();
      } catch (const rust::Error &error) {
        pushError(QStringLiteral("invalid subset string: %1").arg(QString::fromUtf8(error.what())));
        return false;
      }
    }
    mSubset = trimmed;
    mCount = -1;
    mExtent.reset();
    clearMinMaxCache();
    emit dataChanged();
    return true;
  }

 private:
  std::shared_ptr<Shared> mShared;
  bool mValid = false;
  QString mSubset;
  mutable long long mCount = -1;
  mutable std::optional<QgsRectangle> mExtent;
};

class Metadata final : public QgsProviderMetadata {
 public:
  Metadata() : QgsProviderMetadata(PROVIDER_KEY, PROVIDER_DESCRIPTION) {}

  QgsDataProvider *createProvider(const QString &uri, const QgsDataProvider::ProviderOptions &options,
                                  QgsDataProvider::ReadFlags flags) override {
    return new Provider(uri, options, flags);
  }

  QVariantMap decodeUri(const QString &uri) const override {
    const Uri parts = decode(uri);
    QVariantMap map;
    map.insert(QStringLiteral("path"), parts.path);
    if (!parts.layerName.isEmpty()) map.insert(QStringLiteral("layerName"), parts.layerName);
    return map;
  }

  QString encodeUri(const QVariantMap &parts) const override {
    QString uri = parts.value(QStringLiteral("path")).toString();
    const QString layer = parts.value(QStringLiteral("layerName")).toString();
    if (!layer.isEmpty()) uri += QStringLiteral("|layername=%1").arg(layer);
    return uri;
  }

  QgsProviderMetadata::ProviderCapabilities providerCapabilities() const override {
    return QgsProviderMetadata::FileBasedUris;
  }
};

}  // namespace

bool register_provider() {
  QgsProviderRegistry *registry = QgsProviderRegistry::instance();
  if (registry->providerList().contains(PROVIDER_KEY)) return false;
  return registry->registerProvider(new Metadata());
}

}  // namespace gpkgrs
