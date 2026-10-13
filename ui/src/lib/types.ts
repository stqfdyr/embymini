// Emby 回包里界面用到的字段（以真实回包为准，都可能缺失）。

export interface UserData {
  PlaybackPositionTicks?: number
  PlayedPercentage?: number
  Played?: boolean
  IsFavorite?: boolean
  UnplayedItemCount?: number
}

export interface Person {
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
  Bitrate?: number
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
  Status?: string
  CommunityRating?: number | null
  OfficialRating?: string | null
  Overview?: string
  Genres?: string[]
  RunTimeTicks?: number
  ImageTags?: Record<string, string>
  BackdropImageTags?: string[]
  ParentBackdropItemId?: string
  ParentBackdropImageTags?: string[]
  ParentLogoItemId?: string
  ParentLogoImageTag?: string
  SeriesPrimaryImageTag?: string
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
