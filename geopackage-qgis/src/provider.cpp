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
#include <QJsonDocument>
#include <QStringList>
#include <QTime>
#include <QTimeZone>

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
#include "qgsvariantutils.h"
#include "qgsvectordataprovider.h"
#include "qgswkbptr.h"
#include "qgswkbtypes.h"

// QGIS 4 only: it builds against Qt 6, and its enums all live in `Qgis`.
// QGIS 3 users get the plugin's Python provider instead.
#include "qgsconfig.h"
#if _QGIS_VERSION_INT < 40000
#error "the native provider builds against QGIS 4 or later"
#endif

namespace gpkgrs {
namespace {

// The key the provider was registered under: the Python provider's own key
// when the plugin uses this provider in its place, or a key of its own when
// both are registered side by side.
QString PROVIDER_KEY = QStringLiteral("geopackage_rs");
const QString PROVIDER_DESCRIPTION = QStringLiteral("GeoPackage (GeoPackage Toolkit, native)");

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

// A QGIS attribute value as a cell for Rust. Kinds follow `Kind`; anything
// without a kind of its own goes as text, as the Python provider sends it.
Cell toCell(const QVariant &value) {
  Cell cell{Null, 0, 0.0, rust::String(), rust::Vec<uint8_t>()};
  if (QgsVariantUtils::isNull(value)) return cell;
  switch (static_cast<QMetaType::Type>(value.userType())) {
    case QMetaType::Bool:
      cell.kind = Bool;
      cell.int_value = value.toBool() ? 1 : 0;
      break;
    case QMetaType::Int:
    case QMetaType::UInt:
    case QMetaType::LongLong:
    case QMetaType::ULongLong:
    case QMetaType::Short:
    case QMetaType::UShort:
    case QMetaType::Long:
    case QMetaType::ULong:
      cell.kind = Int;
      cell.int_value = value.toLongLong();
      break;
    case QMetaType::Double:
    case QMetaType::Float:
      cell.kind = Double;
      cell.double_value = value.toDouble();
      break;
    case QMetaType::QByteArray: {
      const QByteArray bytes = value.toByteArray();
      cell.kind = Blob;
      cell.blob.reserve(static_cast<size_t>(bytes.size()));
      for (char byte : bytes) cell.blob.push_back(static_cast<uint8_t>(byte));
      break;
    }
    case QMetaType::QDate:
      cell.kind = Date;
      cell.int_value = QDate(1970, 1, 1).daysTo(value.toDate());
      break;
    case QMetaType::QDateTime: {
      QDateTime at = value.toDateTime();
      // A local time has no zone to convert from, so its wall clock is
      // stored, as the Python provider stores a naive datetime. A time with
      // an offset or a zone is converted to UTC.
      at = at.timeSpec() == Qt::LocalTime ? QDateTime(at.date(), at.time(), QTimeZone::utc()) : at.toUTC();
      cell.kind = DateTime;
      cell.int_value = at.toMSecsSinceEpoch() * 1000;
      break;
    }
    case QMetaType::QTime: {
      const QByteArray text = value.toTime().toString(QStringLiteral("HH:mm:ss.zzz")).toUtf8();
      cell.kind = Text;
      cell.text = rust::String(text.constData(), static_cast<size_t>(text.size()));
      break;
    }
    case QMetaType::QVariantList:
    case QMetaType::QStringList:
    case QMetaType::QVariantMap: {
      const QByteArray text = QJsonDocument::fromVariant(value).toJson(QJsonDocument::Compact);
      cell.kind = Text;
      cell.text = rust::String(text.constData(), static_cast<size_t>(text.size()));
      break;
    }
    default: {
      const QByteArray text = value.toString().toUtf8();
      cell.kind = Text;
      cell.text = rust::String(text.constData(), static_cast<size_t>(text.size()));
      break;
    }
  }
  return cell;
}

rust::Vec<uint8_t> toVec(const QByteArray &bytes) {
  rust::Vec<uint8_t> out;
  out.reserve(static_cast<size_t>(bytes.size()));
  for (char byte : bytes) out.push_back(static_cast<uint8_t>(byte));
  return out;
}

// ISO WKB for `geometry`, made multi and given or stripped of z and m to
// match the column, or empty for a null or empty geometry.
QByteArray wkbFor(const QgsGeometry &geometry, Qgis::WkbType target) {
  if (geometry.isNull() || geometry.isEmpty()) return QByteArray();
  QgsGeometry copy(geometry);
  if (QgsWkbTypes::isMultiType(target) && !copy.isMultipart()) copy.convertToMultiType();
  QgsAbstractGeometry *abstract = copy.get();
  if (!QgsWkbTypes::hasZ(target) && abstract->is3D()) {
    abstract->dropZValue();
  } else if (QgsWkbTypes::hasZ(target) && !abstract->is3D()) {
    abstract->addZValue(0);
  }
  if (!QgsWkbTypes::hasM(target) && abstract->isMeasure()) {
    abstract->dropMValue();
  } else if (QgsWkbTypes::hasM(target) && !abstract->isMeasure()) {
    abstract->addMValue(0);
  }
  return copy.asWkb();
}

// The Qt type a field of value kind `kind` holds.
QMetaType::Type metaType(uint8_t kind) {
  switch (kind) {
    case Int: return QMetaType::LongLong;
    case Double: return QMetaType::Double;
    case Bool: return QMetaType::Bool;
    case Blob: return QMetaType::QByteArray;
    case Date: return QMetaType::QDate;
    case DateTime: return QMetaType::QDateTime;
    default: return QMetaType::QString;
  }
}

// A NULL of the field's type, as QGIS's own providers give.
QVariant nullOf(uint8_t kind) { return QgsVariantUtils::createNullVariant(metaType(kind)); }

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
  // A NULL of each field's type, by field index.
  std::vector<QVariant> nulls;
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
    if (!mFilterRect.isNull() && (mRequest.flags() & Qgis::FeatureRequestFlag::ExactIntersect)) {
      mExactGeometry = QgsGeometry::fromRect(mFilterRect);
      mExactEngine.reset(QgsGeometry::createGeometryEngine(mExactGeometry.constGet()));
      mExactEngine->prepareGeometry();
    }
    // An expression filter is evaluated by the base class on every row, and
    // may read any column, so it reads them all.
    const bool expression = mRequest.filterType() == Qgis::FeatureRequestFilterType::Expression;
    mAllColumns = expression || !(mRequest.flags() & Qgis::FeatureRequestFlag::SubsetOfAttributes);
    if (!mAllColumns) {
      for (int index : mRequest.subsetOfAttributes()) {
        if (index >= 0 && index < shared.fields.count()) mColumns.push_back(static_cast<uint32_t>(index));
      }
    }
    mNoGeometry = (mRequest.flags() & Qgis::FeatureRequestFlag::NoGeometry) && mFilterRect.isNull() && !mExactEngine;
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
            attributes[i] = QVariant(QDateTime::fromMSecsSinceEpoch(cursor.int_value(field) / 1000, QTimeZone::utc()));
            break;
          default: attributes[i] = shared.nulls[field]; break;
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
    if (mRequest.filterType() == Qgis::FeatureRequestFilterType::Fid) {
      clauses << QStringLiteral("fid = %1").arg(mRequest.filterFid());
    } else if (mRequest.filterType() == Qgis::FeatureRequestFilterType::Fids) {
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
    const bool byId = mRequest.filterType() == Qgis::FeatureRequestFilterType::Fid || mRequest.filterType() == Qgis::FeatureRequestFilterType::Fids;
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
  Provider(const QString &uri, const ProviderOptions &options, Qgis::DataProviderReadFlags flags)
      : QgsVectorDataProvider(uri, options, flags) {
    const Uri parts = decode(uri);
    try {
      const QByteArray path = parts.path.toUtf8();
      const QByteArray layer = parts.layerName.toUtf8();
      auto handle = open_layer(toStr(path), toStr(layer));
      const LayerInfo info = handle->info();
      auto shared = std::make_shared<Shared>(Shared{std::move(handle), QgsFields(), {}, QgsCoordinateReferenceSystem(),
                                                    Qgis::WkbType::NoGeometry, info.has_geometry});
      for (const FieldInfo &field : info.fields) {
        shared->fields.append(
            QgsField(toQString(field.name), metaType(field.kind), toQString(field.declared_type), field.length));
        shared->nulls.push_back(nullOf(field.kind));
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
  Qgis::VectorProviderCapabilities capabilities() const override {
    Qgis::VectorProviderCapabilities caps = Qgis::VectorProviderCapability::SelectAtId | Qgis::VectorProviderCapability::CircularGeometries;
    if (mValid && mShared->layer->writable()) {
      caps |= Qgis::VectorProviderCapability::AddFeatures | Qgis::VectorProviderCapability::DeleteFeatures | Qgis::VectorProviderCapability::ChangeAttributeValues;
      if (mShared->hasGeometry) caps |= Qgis::VectorProviderCapability::ChangeGeometries;
    }
    return caps;
  }

  bool addFeatures(QgsFeatureList &features, QgsFeatureSink::Flags = QgsFeatureSink::Flags()) override {
    if (!mValid) return false;
    const int width = mShared->fields.count();
    std::vector<NewFeature> rows;
    rows.reserve(static_cast<size_t>(features.size()));
    for (const QgsFeature &feature : std::as_const(features)) {
      NewFeature row{rust::Vec<uint8_t>(), rust::Vec<Cell>()};
      if (mShared->hasGeometry) row.wkb = toVec(wkbFor(feature.geometry(), mShared->wkbType));
      const QgsAttributes attributes = feature.attributes();
      row.cells.reserve(static_cast<size_t>(width));
      for (int i = 0; i < width; ++i) row.cells.push_back(toCell(attributes.value(i)));
      rows.push_back(std::move(row));
    }
    try {
      const rust::Vec<int64_t> fids = mShared->layer->add_features(rust::Slice<const NewFeature>(rows.data(), rows.size()));
      size_t i = 0;
      for (QgsFeature &feature : features) feature.setId(fids[i++]);
    } catch (const rust::Error &error) {
      pushError(QStringLiteral("cannot add features: %1").arg(QString::fromUtf8(error.what())));
      return false;
    }
    edited();
    return true;
  }

  bool deleteFeatures(const QgsFeatureIds &ids) override {
    if (!mValid) return false;
    if (ids.isEmpty()) return true;
    std::vector<int64_t> fids(ids.cbegin(), ids.cend());
    try {
      mShared->layer->delete_features(rust::Slice<const int64_t>(fids.data(), fids.size()));
    } catch (const rust::Error &error) {
      pushError(QStringLiteral("cannot delete features: %1").arg(QString::fromUtf8(error.what())));
      return false;
    }
    edited();
    return true;
  }

  bool changeAttributeValues(const QgsChangedAttributesMap &changes) override {
    if (!mValid) return false;
    std::vector<AttributeChange> flat;
    for (auto feature = changes.cbegin(); feature != changes.cend(); ++feature) {
      for (auto value = feature.value().cbegin(); value != feature.value().cend(); ++value) {
        if (value.key() < 0 || value.key() >= mShared->fields.count()) continue;
        flat.push_back(AttributeChange{feature.key(), static_cast<uint32_t>(value.key()), toCell(value.value())});
      }
    }
    try {
      mShared->layer->change_attributes(rust::Slice<const AttributeChange>(flat.data(), flat.size()));
    } catch (const rust::Error &error) {
      pushError(QStringLiteral("cannot change attribute values: %1").arg(QString::fromUtf8(error.what())));
      return false;
    }
    edited();
    return true;
  }

  bool changeGeometryValues(const QgsGeometryMap &changes) override {
    if (!mValid) return false;
    std::vector<GeometryChange> flat;
    for (auto change = changes.cbegin(); change != changes.cend(); ++change) {
      const QByteArray wkb = wkbFor(change.value(), mShared->wkbType);
      if (wkb.isEmpty()) {
        pushError(QStringLiteral("this provider cannot remove a geometry from a feature"));
        return false;
      }
      flat.push_back(GeometryChange{change.key(), toVec(wkb)});
    }
    try {
      mShared->layer->change_geometries(rust::Slice<const GeometryChange>(flat.data(), flat.size()));
    } catch (const rust::Error &error) {
      pushError(QStringLiteral("cannot change geometries: %1").arg(QString::fromUtf8(error.what())));
      return false;
    }
    edited();
    return true;
  }

  void handlePostCloneOperations(QgsVectorDataProvider *source) override {
    if (auto *other = dynamic_cast<Provider *>(source)) setSubsetString(other->mSubset, false);
  }

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

 protected:
  void reloadProviderData() override { edited(); }

 private:
  // Forgets what an edit, or a change made outside, can make stale.
  void edited() {
    mCount = -1;
    mExtent.reset();
    clearMinMaxCache();
  }

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
                                  Qgis::DataProviderReadFlags flags) override {
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

bool register_provider(rust::Str key) {
  QgsProviderRegistry *registry = QgsProviderRegistry::instance();
  const QString wanted = toQString(key);
  if (registry->providerList().contains(wanted)) return false;
  PROVIDER_KEY = wanted;
  return registry->registerProvider(new Metadata());
}

int32_t built_qgis_version() { return _QGIS_VERSION_INT; }

}  // namespace gpkgrs
