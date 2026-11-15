// Fields of Emby responses the UI reads (declared from real responses; any of them may be missing).

interface UserData {
  PlaybackPositionTicks?: number
  PlayedPercentage?: number
  Played?: boolean
  IsFavorite?: boolean
  UnplayedItemCount?: number
}

interface Person {
  Id: string
  Name: string
  Role?: string | null
  Type: string
  PrimaryImageTag?: string | null
}

export interface MediaStream {
  Index: number
  Type: 'Video' | 'Audio' | 'Subtitle' | string
  Codec?: string
  Language?: string | null
  DisplayTitle?: string
  Title?: string | null
  IsExternal?: boolean
  IsDefault?: boolean
  IsInterlaced?: boolean
  Width?: number
  Height?: number
  VideoRange?: string
  RealFrameRate?: number
  AverageFrameRate?: number
  BitRate?: number
  BitDepth?: number
  Profile?: string
  Level?: number
  AspectRatio?: string
  PixelFormat?: string
  Channels?: number
  ChannelLayout?: string
  SampleRate?: number
}

export interface MediaSource {
  Id: string
  Name?: string
  Container?: string
  Size?: number
  Path?: string
  MediaStreams?: MediaStream[]
}

export interface Item {
  Id: string
  Name: string
  Type: string
  CollectionType?: string
  SeriesId?: string
  SeriesName?: string
  SeasonId?: string
  IndexNumber?: number
  ParentIndexNumber?: number
  ProductionYear?: number
  PremiereDate?: string
  EndDate?: string | null
  /** Person: birthplace. */
  ProductionLocations?: string[]
  Status?: string
  /** Series: episode count (seasons not counted). */
  RecursiveItemCount?: number
  CommunityRating?: number | null
  OfficialRating?: string | null
  Overview?: string
  Genres?: string[]
  RunTimeTicks?: number
  ImageTags?: Record<string, string>
  BackdropImageTags?: string[]
  ParentBackdropItemId?: string
  ParentBackdropImageTags?: string[]
  UserData?: UserData
  People?: Person[]
  ExternalUrls?: { Name: string; Url: string }[]
  ProviderIds?: Record<string, string>
  MediaSources?: MediaSource[]
}

export interface Page<T = Item> {
  Items: T[]
  TotalRecordCount: number
}
